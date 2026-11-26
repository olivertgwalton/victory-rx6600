//! Under Wine our own HLSL (the indirect-argument clamp, the card compare,
//! depth planes, the upscaler) goes to DXC. Wine's `d3dcompiler_47`
//! (vkd3d-shader) has no `RWByteAddressBuffer` loads or atomics and ignores
//! `[RootSignature]`, so those shaders either fail to compile or come back
//! without the root signature their pipelines are made from. DXC compiles
//! the same sources with their root signatures (shader model 6.0 for 5.1).
//! `dxcompiler.dll` (with its `dxil.dll`) is looked for at `VICTORY_DXC`
//! (the DLL or its folder), then beside the executable. Our callers use
//! [`D3DCompile`] here in place of the system's, with the same arguments.

use std::sync::OnceLock;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
};
use windows::core::{HSTRING, PCSTR, PCWSTR, s, w};

/// Whether this process runs under Wine (its `ntdll` exports
/// `wine_get_version`).
pub(crate) fn under_wine() -> bool {
    static WINE: OnceLock<bool> = OnceLock::new();
    *WINE.get_or_init(|| {
        // SAFETY: plain module and export queries.
        unsafe {
            GetModuleHandleW(w!("ntdll.dll"))
                .ok()
                .is_some_and(|m| GetProcAddress(m, s!("wine_get_version")).is_some())
        }
    })
}

/// Under Wine, `source`'s function `entry` compiled by DXC for `target`
/// (a 5.1 target becomes 6.0); elsewhere `None`, for the system's compiler.
pub(crate) fn compile(
    source: &str,
    entry: PCSTR,
    target: PCSTR,
) -> Option<windows::core::Result<Vec<u8>>> {
    if !under_wine() {
        return None;
    }
    // SAFETY: both are NUL-terminated literals of the callers.
    let (entry, target) = unsafe { (entry.to_string().ok()?, target.to_string().ok()?) };
    let target = target.replace("_5_1", "_6_0").replace("_5_0", "_6_0");
    Some(dxc(source, &entry, &target).inspect_err(|e| {
        tracing::warn!(error = %e, entry, target, "a shader of ours: DXC failed");
    }))
}

fn library() -> windows::core::Result<windows::Win32::Foundation::HMODULE> {
    if let Some(p) = std::env::var_os("VICTORY_DXC").map(std::path::PathBuf::from) {
        let path = if p.is_dir() { p.join("dxcompiler.dll") } else { p };
        // SAFETY: a NUL-terminated path; its folder (dxil.dll) is searched first.
        if let Ok(m) = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        } {
            return Ok(m);
        }
    }
    // SAFETY: a bare name, searched in the executable's folder.
    unsafe { LoadLibraryExW(w!("dxcompiler.dll"), None, LOAD_LIBRARY_SEARCH_APPLICATION_DIR) }
}

fn dxc(source: &str, entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::core::Interface;
    type Create = unsafe extern "system" fn(
        *const windows::core::GUID,
        *const windows::core::GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    let (t, e) = (HSTRING::from(target), HSTRING::from(entry));
    // SAFETY: loading the compiler and calling its documented entry point
    // with out pointers of ours; the buffer and arguments outlive the calls.
    unsafe {
        let module = library()?;
        let create: Create = std::mem::transmute(
            GetProcAddress(module, s!("DxcCreateInstance"))
                .ok_or_else(windows::core::Error::from_thread)?,
        );
        let mut raw = std::ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw).ok()?;
        let compiler = IDxcCompiler3::from_raw(raw);
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let arguments = [w!("-T"), PCWSTR(t.as_ptr()), w!("-E"), PCWSTR(e.as_ptr()), w!("-O3")];
        let result: IDxcResult = compiler.Compile(&buffer, Some(&arguments), None)?;
        if let Err(err) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!("a shader of ours: DXC: {}", String::from_utf8_lossy(text).trim());
            }
            return Err(err);
        }
        let blob = result.GetResult()?;
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
            .to_vec())
    }
}

/// `D3DCompile` with the system compiler's arguments: under Wine, DXC's
/// bytes in a blob of the system's (`include` and `defines` are not used by
/// any caller, and DXC gets neither); elsewhere the system's own.
#[allow(non_snake_case, clippy::too_many_arguments)]
pub(crate) unsafe fn D3DCompile(
    source: *const std::ffi::c_void,
    len: usize,
    name: impl windows::core::Param<PCSTR>,
    defines: Option<*const windows::Win32::Graphics::Direct3D::D3D_SHADER_MACRO>,
    include: Option<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
    entry: PCSTR,
    target: PCSTR,
    flags1: u32,
    flags2: u32,
    code: *mut Option<windows::Win32::Graphics::Direct3D::ID3DBlob>,
    errors: Option<*mut Option<windows::Win32::Graphics::Direct3D::ID3DBlob>>,
) -> windows::core::Result<()> {
    use windows::Win32::Graphics::Direct3D::Fxc;
    if under_wine() {
        // SAFETY: the caller's source, `len` bytes.
        let text = unsafe { std::slice::from_raw_parts(source.cast::<u8>(), len) };
        let text = std::str::from_utf8(text)
            .map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
        let bytes = compile(text, entry, target).unwrap_or_else(|| Err(windows::core::Error::empty()))?;
        // SAFETY: a blob of the bytes' size, filled before it is handed out.
        unsafe {
            let blob = Fxc::D3DCreateBlob(bytes.len())?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), blob.GetBufferPointer().cast::<u8>(), bytes.len());
            *code = Some(blob);
        }
        return Ok(());
    }
    // SAFETY: the caller's arguments, passed on.
    unsafe { Fxc::D3DCompile(source, len, name, defines, include, entry, target, flags1, flags2, code, errors) }
}
