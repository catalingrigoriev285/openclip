//! A minimal Direct3D 11 application to test game capture against.
//!
//! Everything in `src/game/` and the hook DLL needs a real swapchain presenting
//! real frames to exercise, and requiring a AAA title to test a recorder is not
//! a workable position. This is that target: a window, a swapchain, and a
//! colour that changes every frame so it is obvious whether the picture is live.
//!
//! ```sh
//! cargo run --example gfx_sandbox                      # windowed, uncapped
//! cargo run --example gfx_sandbox -- --vsync           # capped to the display
//! cargo run --example gfx_sandbox -- --resize-after 5  # resize mid-run
//! ```
//!
//! Then, from another terminal:
//!
//! ```sh
//! cargo run --example inject_test -- --exe gfx_sandbox
//! ```

#[cfg(not(windows))]
fn main() {
    eprintln!("the graphics sandbox is Windows-only");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(windows)]
mod win {
    use std::time::Instant;

    use anyhow::{anyhow, Result};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        IDXGISwapChain, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    struct Args {
        vsync: bool,
        resize_after: Option<f32>,
        size: (i32, i32),
        verify: bool,
    }

    fn parse_args() -> Args {
        let mut args = Args { vsync: false, resize_after: None, size: (1280, 720), verify: false };
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--vsync" => args.vsync = true,
                "--verify" => args.verify = true,
                "--resize-after" => args.resize_after = it.next().and_then(|v| v.parse().ok()),
                "--size" => {
                    if let Some(v) = it.next()
                        && let Some((w, h)) = v.split_once('x')
                        && let (Ok(w), Ok(h)) = (w.parse(), h.parse())
                    {
                        args.size = (w, h);
                    }
                }
                other => eprintln!("ignoring unknown flag {other}"),
            }
        }
        args
    }

    pub fn run() -> Result<()> {
        let args = parse_args();
        let hwnd = create_window(args.size)?;
        let (device, context, swap) = create_device(hwnd, args.size)?;
        let mut rtv = render_target(&device, &swap)?;

        println!("gfx_sandbox: pid {} hwnd {:#x}", std::process::id(), hwnd.0 as isize);
        // Also links the openclip library, which is where `build.rs` expects the
        // hybrid-graphics export symbols it adds to every example to come from.
        match openclip::game::hook_dll_path() {
            Some(p) => println!("hook component: {}", p.display()),
            None => println!("hook component: not built — run `cargo build` first"),
        }
        println!("inject with:  cargo run --example inject_test -- --pid {}", std::process::id());

        let start = Instant::now();
        let mut frames = 0u64;
        let mut last_report = Instant::now();
        let mut resized = false;

        loop {
            if !pump_messages() {
                break;
            }
            let t = start.elapsed().as_secs_f32();

            // Resize on demand: this is the path that breaks naive overlays,
            // because ResizeBuffers fails if anything still holds a back buffer.
            if let Some(after) = args.resize_after
                && !resized
                && t >= after
            {
                resized = true;
                drop(rtv);
                println!("resizing the swapchain");
                // SAFETY: no outstanding back-buffer references — `rtv` is gone.
                unsafe { swap.ResizeBuffers(0, 960, 540, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))? };
                rtv = render_target(&device, &swap)?;
            }

            // A colour that moves, so a frozen picture is obvious at a glance.
            // Under --verify it stays a flat dark grey instead, so anything
            // strongly coloured in the buffer can only have come from an overlay.
            let clear = if args.verify {
                [0.05, 0.05, 0.05, 1.0]
            } else {
                [0.5 + 0.5 * (t * 0.7).sin(), 0.5 + 0.5 * (t * 1.1).sin(), 0.5 + 0.5 * (t * 1.7).sin(), 1.0]
            };
            // SAFETY: plain D3D11 rendering on our own device.
            unsafe {
                context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                context.ClearRenderTargetView(&rtv, &clear);
                swap.Present(u32::from(args.vsync), DXGI_PRESENT(0)).ok()?;
            }

            frames += 1;
            if last_report.elapsed().as_secs_f32() >= 1.0 {
                print!("{:.0} fps", frames as f32 / last_report.elapsed().as_secs_f32());
                if args.verify {
                    match scan_overlay(&device, &context, &swap) {
                        Ok(found) => print!("   {found}"),
                        Err(e) => print!("   scan failed: {e}"),
                    }
                }
                println!();
                frames = 0;
                last_report = Instant::now();
            }
        }
        Ok(())
    }

    /// `false` once the window has been closed.
    fn pump_messages() -> bool {
        let mut msg = MSG::default();
        // SAFETY: standard non-blocking message pump.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        true
    }

    extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // SAFETY: the default handler, plus a quit on close.
        unsafe {
            if msg == WM_DESTROY {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
    }

    fn create_window(size: (i32, i32)) -> Result<HWND> {
        let class = HSTRING::from("openclip_gfx_sandbox");
        // SAFETY: registering a class and creating a top-level window.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(anyhow!("registering the window class failed"));
            }
            let hwnd = CreateWindowExW(
                Default::default(),
                PCWSTR(class.as_ptr()),
                &HSTRING::from("openclip graphics sandbox"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                size.0,
                size.1,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            Ok(hwnd)
        }
    }

    fn create_device(
        hwnd: HWND,
        size: (i32, i32),
    ) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGISwapChain)> {
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: size.0 as u32,
                Height: size.1 as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };
        let mut swap = None;
        let mut device = None;
        let mut context = None;
        // SAFETY: standard device + swapchain creation.
        unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok((
            device.ok_or_else(|| anyhow!("no device"))?,
            context.ok_or_else(|| anyhow!("no context"))?,
            swap.ok_or_else(|| anyhow!("no swapchain"))?,
        ))
    }

    /// Reads the back buffer and reports whether openclip's counter is in it.
    ///
    /// This is how the overlay is verified without a screenshot: the hook draws
    /// into the back buffer *before* chaining to the real `Present`, so anything
    /// it painted is in the buffer we read here. With `--verify` the frame is
    /// cleared to a flat dark grey, so a saturated green or red pixel cannot
    /// have come from anywhere else.
    fn scan_overlay(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        swap: &IDXGISwapChain,
    ) -> Result<String> {
        // SAFETY: a staging copy of the back buffer, mapped for read and
        // unmapped before returning.
        unsafe {
            let back: ID3D11Texture2D = swap.GetBuffer(0)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut desc);
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let mut staging = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;
            context.CopyResource(&staging, &back);

            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let (mut green, mut red) = (0u32, 0u32);
            for y in 0..desc.Height as usize {
                let row = (m.pData as *const u8).add(y * m.RowPitch as usize);
                for x in 0..desc.Width as usize {
                    // The swapchain is BGRA.
                    let px = row.add(x * 4);
                    let (b, g, r) = (*px as i32, *px.add(1) as i32, *px.add(2) as i32);
                    if g > 140 && r < 120 && b < 140 {
                        green += 1;
                    } else if r > 200 && g < 130 && b < 130 {
                        red += 1;
                    }
                }
            }
            context.Unmap(&staging, 0);

            Ok(match (green, red) {
                (0, 0) => "overlay: not found".to_string(),
                (g, 0) => format!("overlay: GREEN (ready), {g} px"),
                (0, r) => format!("overlay: RED (recording), {r} px"),
                (g, r) => format!("overlay: {g} green + {r} red px"),
            })
        }
    }

    fn render_target(device: &ID3D11Device, swap: &IDXGISwapChain) -> Result<ID3D11RenderTargetView> {
        // SAFETY: the back buffer of a live swapchain.
        unsafe {
            let back: ID3D11Texture2D = swap.GetBuffer(0)?;
            let mut rtv = None;
            device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            rtv.ok_or_else(|| anyhow!("no render target view"))
        }
    }
}
