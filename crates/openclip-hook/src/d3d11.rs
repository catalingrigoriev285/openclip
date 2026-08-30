//! Direct3D 11: the overlay renderer, and the surface that hands DXGI the
//! game's own back buffer.
//!
//! The renderer here is deliberately *not* tied to a swapchain. It draws onto
//! and publishes from whatever [`crate::dxgi::Target`] it is given, which is
//! what lets the D3D12 path reuse every line of it through a D3D11On12 wrapper
//! rather than growing a second copy of the same shader pipeline.

use openclip_overlay::abi::{GfxApi, OverlaySettings};
use openclip_overlay::fps::{self, FpsBadge, HookState};
use openclip_overlay::layout::Corner;
use windows::core::HRESULT;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;

use crate::dxgi::{Surface, Target};
use crate::logging::hlog;
use crate::publish::{self, Publisher};

// ----- the surface -----------------------------------------------------------

/// The back buffer of a Direct3D 11 swapchain, handed over as it is.
///
/// The cheapest possible surface: the game's device *is* the device the overlay
/// draws with, so there is nothing to wrap, acquire or release. The render
/// target is cached because a D3D11 swapchain's buffer 0 is the same texture
/// every frame until `ResizeBuffers`, which drops the whole surface anyway.
pub struct Surface11 {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    back: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    size: (u32, u32),
}

impl Surface11 {
    /// `None` when the swapchain is not a Direct3D 11 one.
    pub fn for_swapchain(swap: &IDXGISwapChain) -> Option<Self> {
        // SAFETY: `swap` is the live swapchain the game just presented on;
        // `GetDevice` is a QueryInterface and fails cleanly for D3D12.
        unsafe {
            let device: ID3D11Device = swap.GetDevice().ok()?;
            let context = device.GetImmediateContext().ok()?;
            let back: ID3D11Texture2D = swap.GetBuffer(0).ok()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut desc);
            let mut rtv = None;
            device.CreateRenderTargetView(&back, None, Some(&mut rtv)).ok()?;
            Some(Self { device, context, back, rtv: rtv?, size: (desc.Width, desc.Height) })
        }
    }
}

impl Surface for Surface11 {
    fn api(&self) -> GfxApi {
        GfxApi::D3D11
    }

    fn acquire(&mut self, _swap: &IDXGISwapChain) -> windows::core::Result<Target> {
        Ok(Target {
            device: self.device.clone(),
            context: self.context.clone(),
            back: self.back.clone(),
            rtv: self.rtv.clone(),
            size: self.size,
        })
    }

    fn release(&mut self) {}

    fn release_swapchain_resources(&mut self) {}
}

// ----- the overlay renderer --------------------------------------------------

pub(crate) struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    badge: FpsBadge,
    /// The uploaded sprite and the dimensions it was made for.
    texture: Option<(u32, u32, ID3D11Texture2D, ID3D11ShaderResourceView)>,
    /// What the uploaded texture currently shows.
    shown: Option<(String, [u8; 3])>,
    /// Hands the back buffer to openclip; `None` until it is first needed.
    publisher: Option<Publisher>,
    /// Set once a back-buffer format has been refused, so the note is not
    /// rewritten on every present.
    format_refused: bool,
}

impl Renderer {
    /// Builds the shader pipeline on `device`.
    ///
    /// Nothing here is swapchain-specific — no render target, no size — because
    /// the same renderer serves a D3D11 game directly and a D3D12 one through a
    /// D3D11On12 wrapper, and those disagree about everything except the device.
    pub(crate) fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> windows::core::Result<Self> {
        // SAFETY: standard D3D11 resource creation on the game's own device.
        unsafe {
            let mut vs = None;
            device.CreateVertexShader(include_bytes!("../shaders/overlay_vs.dxbc"), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(include_bytes!("../shaders/overlay_ps.dxbc"), None, Some(&mut ps))?;

            // Straight alpha, which is what the sprite carries.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&blend_desc, Some(&mut blend))?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: 32, // two float4 registers
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            device.CreateBuffer(&cb_desc, None, Some(&mut constants))?;

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.expect("created above"),
                ps: ps.expect("created above"),
                blend: blend.expect("created above"),
                sampler: sampler.expect("created above"),
                constants: constants.expect("created above"),
                badge: FpsBadge::new().ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?,
                texture: None,
                shown: None,
                publisher: None,
                format_refused: false,
            })
        }
    }

    /// Whether this renderer belongs to `device`, so a device change rebuilds it
    /// rather than issuing calls against objects from a dead one.
    pub(crate) fn matches(&self, device: &ID3D11Device) -> bool {
        use windows::core::Interface;
        self.device.as_raw() == device.as_raw()
    }

    /// Copies the back buffer into openclip's shared texture.
    ///
    /// `Ok(false)` means the frame was deliberately skipped — the rate limiter
    /// dropped it, openclip was still reading the slot, or the format is one the
    /// pipeline cannot take. None of those is an error worth tearing down for.
    pub(crate) fn publish(
        &mut self,
        control: &openclip_overlay::abi::Control,
        target: &Target,
        now: i64,
    ) -> windows::core::Result<bool> {
        let back = &target.back;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `back` is the surface's own back buffer.
        unsafe { back.GetDesc(&mut desc) };

        if !openclip_overlay::abi::format_supported(desc.Format.0 as u32) {
            if !self.format_refused {
                self.format_refused = true;
                publish::report_unsupported_format(control, desc.Format);
                hlog!("d3d11: back buffer format {} cannot be recorded", desc.Format.0);
            }
            return Ok(false);
        }

        let publisher = self
            .publisher
            .get_or_insert_with(|| Publisher::new(self.device.clone(), self.context.clone()));
        publisher.publish(control, back, now)
    }

    /// Releases everything that references the swapchain's buffers.
    pub(crate) fn release_swapchain_resources(&mut self) {
        if let Some(p) = &mut self.publisher {
            p.release();
        }
    }

    pub(crate) fn draw(
        &mut self,
        target: &Target,
        fps: f32,
        state: HookState,
        settings: OverlaySettings,
    ) -> windows::core::Result<()> {
        let (fw, fh) = target.size;
        let overlay = fps::FpsOverlay {
            enabled: settings.enabled,
            position: Corner::ALL[(settings.corner as usize).min(3)],
            size: settings.size as u32,
            opacity: settings.opacity as u32,
            in_recording: settings.burn_in,
        };
        let height = overlay.badge_height(fh);
        let text = fps::format_fps(fps);
        let rgb = state.color();

        let sprite = self.badge.sprite_for(height, &text, rgb);
        let (sw, sh) = (sprite.width, sprite.height);
        let Some((x, y)) = overlay.place((sw, sh), (fw, fh)) else {
            return Ok(()); // too small a window to put a counter on
        };
        // `sprite` borrows `self.badge`; copy what the upload needs and let go.
        let changed = self.shown.as_ref().is_none_or(|(t, c)| t != &text || c != &rgb);
        if changed {
            let pixels = sprite.rgba.clone();
            self.upload(sw, sh, &pixels)?;
            self.shown = Some((text, rgb));
        }
        let Some((_, _, _, srv)) = &self.texture else { return Ok(()) };
        let srv = srv.clone();

        // Clip space: x right, y up, origin centre.
        let ndc = [
            (x as f32 / fw as f32) * 2.0 - 1.0,
            1.0 - (y as f32 / fh as f32) * 2.0,
            (sw as f32 / fw as f32) * 2.0,
            -(sh as f32 / fh as f32) * 2.0,
        ];
        let opacity = (overlay.opacity.min(100) as f32) / 100.0;

        // SAFETY: every call below is on the game's immediate context, wrapped
        // by a state block that puts back everything we touch.
        unsafe {
            let mapped = {
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(&self.constants, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                m
            };
            let data = mapped.pData as *mut f32;
            std::ptr::copy_nonoverlapping(ndc.as_ptr(), data, 4);
            *data.add(4) = opacity;
            *data.add(5) = 0.0;
            *data.add(6) = 0.0;
            *data.add(7) = 0.0;
            self.context.Unmap(&self.constants, 0);

            let saved = StateBlock::capture(&self.context);
            self.set_state(&target.rtv, &srv, fw, fh);
            self.context.Draw(4, 0);
            saved.restore(&self.context);
        }
        Ok(())
    }

    /// Binds our pipeline. Everything set here is captured by [`StateBlock`].
    ///
    /// The quad comes from `SV_VertexID`, so there is no vertex buffer and no
    /// input layout to bind — one less piece of state to get wrong.
    unsafe fn set_state(&self, rtv: &ID3D11RenderTargetView, srv: &ID3D11ShaderResourceView, fw: u32, fh: u32) {
        let ctx = &self.context;
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: fw as f32,
                Height: fh as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.RSSetState(None);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.OMSetBlendState(&self.blend, Some(&[0.0; 4]), 0xffff_ffff);
            ctx.OMSetDepthStencilState(None, 0);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            // A geometry shader would run over our triangles and draw nonsense.
            // Hull and domain shaders are ignored for a non-patch topology, so
            // they can be left exactly as the game had them.
            ctx.GSSetShader(None, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
    }

    /// Puts the composed sprite into a texture the shader can sample.
    fn upload(&mut self, w: u32, h: u32, rgba: &[u8]) -> windows::core::Result<()> {
        let fits = matches!(&self.texture, Some((tw, th, ..)) if *tw == w && *th == h);
        if !fits {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            // SAFETY: a plain 2D texture plus its view, both checked below.
            unsafe {
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.expect("created above");
                let mut srv = None;
                self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
                self.texture = Some((w, h, tex, srv.expect("created above")));
            }
        }
        let Some((_, _, tex, _)) = &self.texture else { return Ok(()) };
        // SAFETY: dynamic texture, mapped for write and unmapped below; the row
        // loop respects the pitch the driver hands back.
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            let row = w as usize * 4;
            for y in 0..h as usize {
                let dst = (m.pData as *mut u8).add(y * m.RowPitch as usize);
                std::ptr::copy_nonoverlapping(rgba.as_ptr().add(y * row), dst, row);
            }
            self.context.Unmap(tex, 0);
        }
        Ok(())
    }
}

/// Everything [`Renderer::set_state`] touches, so the game gets it all back.
///
/// The `*Get*` methods add a reference to whatever they return; the `windows`
/// crate's smart pointers release it on drop, which removes the leak that hand-
/// written versions of this in C++ are famous for.
struct StateBlock {
    viewports: Vec<D3D11_VIEWPORT>,
    rasterizer: Option<ID3D11RasterizerState>,
    render_targets: [Option<ID3D11RenderTargetView>; 8],
    depth: Option<ID3D11DepthStencilView>,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    depth_stencil: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    input_layout: Option<ID3D11InputLayout>,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    vs: Option<ID3D11VertexShader>,
    ps: Option<ID3D11PixelShader>,
    gs: Option<ID3D11GeometryShader>,
    vs_cb: [Option<ID3D11Buffer>; 1],
    ps_cb: [Option<ID3D11Buffer>; 1],
    ps_srv: [Option<ID3D11ShaderResourceView>; 1],
    ps_sampler: [Option<ID3D11SamplerState>; 1],
}

impl StateBlock {
    /// # Safety
    /// `ctx` must be the immediate context the draw will run on.
    unsafe fn capture(ctx: &ID3D11DeviceContext) -> Self {
        unsafe {
            // One more than the maximum index: the full set D3D11 can hold.
            let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_MAX_INDEX + 1;
            let mut viewports = vec![D3D11_VIEWPORT::default(); count as usize];
            ctx.RSGetViewports(&mut count, Some(viewports.as_mut_ptr()));
            viewports.truncate(count as usize);

            let mut render_targets: [Option<ID3D11RenderTargetView>; 8] = Default::default();
            let mut depth = None;
            ctx.OMGetRenderTargets(Some(&mut render_targets), Some(&mut depth));

            let mut blend = None;
            let mut blend_factor = [0.0f32; 4];
            let mut sample_mask = 0u32;
            ctx.OMGetBlendState(Some(&mut blend), Some(&mut blend_factor), Some(&mut sample_mask));

            let mut depth_stencil = None;
            let mut stencil_ref = 0u32;
            ctx.OMGetDepthStencilState(Some(&mut depth_stencil), Some(&mut stencil_ref));

            let topology = ctx.IAGetPrimitiveTopology();
            let input_layout = ctx.IAGetInputLayout().ok();

            let mut vs = None;
            ctx.VSGetShader(&mut vs, None, None);
            let mut ps = None;
            ctx.PSGetShader(&mut ps, None, None);
            let mut gs = None;
            ctx.GSGetShader(&mut gs, None, None);

            let mut vs_cb: [Option<ID3D11Buffer>; 1] = Default::default();
            ctx.VSGetConstantBuffers(0, Some(&mut vs_cb));
            let mut ps_cb: [Option<ID3D11Buffer>; 1] = Default::default();
            ctx.PSGetConstantBuffers(0, Some(&mut ps_cb));
            let mut ps_srv: [Option<ID3D11ShaderResourceView>; 1] = Default::default();
            ctx.PSGetShaderResources(0, Some(&mut ps_srv));
            let mut ps_sampler: [Option<ID3D11SamplerState>; 1] = Default::default();
            ctx.PSGetSamplers(0, Some(&mut ps_sampler));

            Self {
                viewports,
                rasterizer: ctx.RSGetState().ok(),
                render_targets,
                depth,
                blend,
                blend_factor,
                sample_mask,
                depth_stencil,
                stencil_ref,
                input_layout,
                topology,
                vs,
                ps,
                gs,
                vs_cb,
                ps_cb,
                ps_srv,
                ps_sampler,
            }
        }
    }

    /// # Safety
    /// Must be called on the same context [`capture`](Self::capture) read.
    unsafe fn restore(self, ctx: &ID3D11DeviceContext) {
        unsafe {
            ctx.RSSetViewports(Some(&self.viewports));
            ctx.RSSetState(self.rasterizer.as_ref());
            ctx.OMSetRenderTargets(Some(&self.render_targets), self.depth.as_ref());
            ctx.OMSetBlendState(self.blend.as_ref(), Some(&self.blend_factor), self.sample_mask);
            ctx.OMSetDepthStencilState(self.depth_stencil.as_ref(), self.stencil_ref);
            ctx.IASetInputLayout(self.input_layout.as_ref());
            ctx.IASetPrimitiveTopology(self.topology);
            ctx.VSSetShader(self.vs.as_ref(), None);
            ctx.PSSetShader(self.ps.as_ref(), None);
            ctx.GSSetShader(self.gs.as_ref(), None);
            ctx.VSSetConstantBuffers(0, Some(&self.vs_cb));
            ctx.PSSetConstantBuffers(0, Some(&self.ps_cb));
            ctx.PSSetShaderResources(0, Some(&self.ps_srv));
            ctx.PSSetSamplers(0, Some(&self.ps_sampler));
        }
    }
}
