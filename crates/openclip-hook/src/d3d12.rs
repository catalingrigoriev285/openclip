//! Direct3D 12 detection.
//!
//! A D3D12 game presents through the very same `IDXGISwapChain` vtable this hook
//! already patches, so [`crate::dxgi`]'s `Present` hook fires for one exactly as
//! it does for D3D11 — the counter is even measured correctly. What is missing
//! is a way to *touch* the back buffer: the device behind the swapchain is an
//! `ID3D12Device`, the back buffers are `ID3D12Resource`s, and there is no
//! `GetCommandQueue` on a swapchain to submit work with. Bridging that needs
//! D3D11On12 over a command queue captured from
//! `ID3D12CommandQueue::ExecuteCommandLists`, which is its own piece of work.
//!
//! Until then this exists to *recognise* the case, so a D3D12 game gets a
//! specific note in the status card instead of being mistaken for D3D11 and
//! failing somewhere far less legible.

use windows::Win32::Graphics::Dxgi::IDXGISwapChain;

/// Whether the device behind `swap` is a Direct3D 12 one.
pub fn is_d3d12(swap: &IDXGISwapChain) -> bool {
    // `GetDevice` is generic over the interface asked for, so this is a plain
    // QueryInterface: it succeeds only for a device that really is D3D12.
    // SAFETY: `swap` is the live swapchain the game just presented on.
    unsafe { swap.GetDevice::<windows::Win32::Graphics::Direct3D12::ID3D12Device>() }.is_ok()
}
