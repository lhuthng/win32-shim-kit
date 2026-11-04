//! `d3d8.dll`: the device a pre-shader 3D guest renders through.
//!
//! `d3d8.dll` exports exactly one function, `Direct3DCreate8`; everything else
//! the guest calls arrives through the COM vtables that function hands out.
//! That makes the vtable index the ABI: a table one name short calls the wrong
//! method with the wrong arguments, and nothing about the call site would say so.
//! The twelve tables below are therefore the whole surface, in `d3d8.h` order,
//! from `IDirect3D8`'s sixteen slots to `IDirect3DDevice8`'s ninety-seven.
//!
//! # What this module answers
//!
//! `Direct3DCreate8` (the SDK version has to be 220, which is what an image built
//! against the 8.1 SDK passes), `QueryInterface` for `IID_IUnknown`,
//! `AddRef`/`Release`, and the fixed-function device: adapter queries,
//! `CreateDevice`, `Reset`, `Present`, the render and texture-stage state,
//! transforms, the viewport, vertex and index buffers with their locks, level 0
//! of a texture, what a surface says about itself through `GetDesc`, and the
//! four draw calls.
//!
//! # The page draws, not this module
//!
//! A shim runs inside the wasm module; the picture is drawn by a WebGL2 backend
//! on the page's side of the boundary. So every answered method does two things
//! that cannot be separated: it updates the device state a later draw reads, and
//! it appends a record to [`crate::render`]'s command stream. The stream is a log
//! of changes, not a frame — replaying a draw before the transform it depends on
//! draws in the wrong place, which is why the order is written in call order.
//! Vertex and texture *bytes* are not in the stream: a record carries a guest
//! offset, and the page reads linear memory itself.
//!
//! Lighting, cube and volume textures, render targets, shaders, state blocks and
//! read-back are refused by name, as is anything else not listed above:
//! `IDirect3DDevice8.CreateCubeTexture has no shim`. That is the porting loop this
//! kit runs on — the first method a guest needs names itself, rather than a guess
//! about which method is "probably" next corrupting state three frames later.
//! Materials and lights are the exception: they are accepted, because a game sets
//! them before its first draw and a refusal would stop the run before anything is
//! on screen. One notice says the picture is unlit.
//!
//! The argument counts are the header's, for every slot rather than only the
//! answered ones, because a pop is what the caller's stack depends on the moment
//! a method starts being answered. They were parsed from `d3d8.h`; re-derive them
//! from the declaration rather than from what a shim happens to read.

use std::collections::HashMap;

use crate::render::op;
use crate::shim::{Action, Binding, Call, Host};
use crate::thunks::{
    self, D3D8, D3D8_BASE_TEXTURE, D3D8_CUBE_TEXTURE, D3D8_DEVICE, D3D8_INDEX_BUFFER,
    D3D8_RESOURCE, D3D8_SURFACE, D3D8_SWAP_CHAIN, D3D8_TEXTURE, D3D8_VERTEX_BUFFER, D3D8_VOLUME,
    D3D8_VOLUME_TEXTURE,
};
use crate::Result;

use Action::Return;

const D3D8_DLL: &str = "d3d8.dll";

/// `S_OK`.
const S_OK: u32 = 0;
/// `E_NOINTERFACE`.
const E_NOINTERFACE: u32 = 0x8000_4002;
/// `D3DERR_INVALIDCALL`.
const D3DERR_INVALIDCALL: u32 = 0x8876_0873;

/// `D3D_SDK_VERSION` for the 8.1 runtime. `Direct3DCreate8` answers `NULL` to any
/// other number, which is how a build against a different SDK is told apart.
const D3D_SDK_VERSION: u32 = 220;

/// `IID_IUnknown`, the one interface every object offers.
const IID_UNKNOWN: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

/// What one vertex buffer or index buffer is: where its guest bytes are, and the
/// locks open on it.
///
/// The guest writes vertices through a lock, so the bytes are guest memory the
/// host allocated. A draw names the buffer; the page reads the bytes by guest
/// offset, so nothing is copied through the command stream.
pub struct Buffer {
    pub size: u32,
    pub address: u32,
    /// The lock ranges in the order they were taken, so `Unlock` reports the
    /// range the guest just wrote rather than the whole buffer.
    pub locks: Vec<(u32, u32)>,
    /// `FVF` for a vertex buffer, the index format for an index buffer.
    pub format: u32,
}

/// A texture: every level of the chain the guest asked for, laid out in one
/// guest allocation the way Direct3D lays out a managed texture.
///
/// The page samples level 0, so the extra levels are never *drawn* from -- but
/// a guest fills them through `LockRect` and their surfaces, and the bytes it
/// writes are real memory it can read back, which is what keeps its loaders
/// honest.
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub format: u32,
    /// The address of level 0; level *n* sits `level_sizes[..n]` bytes in.
    pub address: u32,
    /// Level 0's bytes, which is what a `D3DSURFACE_DESC.Size` for the texture
    /// itself reports.
    pub size: u32,
    /// Rows are DWORD-aligned, as Direct3D's are; a guest computes its writes
    /// from the pitch `LockRect` hands back, so it has to be a real pitch.
    pub pitch: u32,
    /// Each level's byte count and row pitch, level 0 first.
    pub level_sizes: Vec<u32>,
    pub level_pitches: Vec<u32>,
}

impl Texture {
    /// The address level `level` starts at, or `None` past the chain.
    pub fn level_address(&self, level: u32) -> Option<u32> {
        let level = level as usize;
        if level >= self.level_sizes.len() {
            return None;
        }
        Some(self.address + self.level_sizes[..level].iter().sum::<u32>())
    }

    fn level_width(&self, level: u32) -> u32 {
        (self.width >> level).max(1)
    }

    fn level_height(&self, level: u32) -> u32 {
        (self.height >> level).max(1)
    }
}

/// One stream source: the vertex buffer bound to it, the stride, and the byte
/// offset a draw starts from.
#[derive(Clone, Copy)]
pub struct Source {
    pub buffer: u32,
    pub stride: u32,
    pub base: u32,
}

/// A surface the device handed out that is not the back buffer: a render
/// target, a depth-stencil, or a plain image. The page draws into one buffer,
/// so what is kept here is what a guest reads back about it with `GetDesc`,
/// not pixels.
#[derive(Clone, Copy)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// The pool the surface lives in. Render targets and depth-stencils are
    /// always `D3DPOOL_DEFAULT`, and an image surface is always system memory:
    /// `CreateImageSurface` takes no pool parameter to choose otherwise.
    pub pool: u32,
    /// `D3DUSAGE_RENDERTARGET` or `D3DUSAGE_DEPTHSTENCIL` for the two the
    /// device draws into, and zero for an image a guest fills itself.
    pub usage: u32,
}

/// The device's own state, which every draw reads.
pub struct Device {
    pub width: u32,
    pub height: u32,
    pub bits: u32,
    /// The `AutoDepthStencilFormat` the guest asked for at `CreateDevice` or
    /// `Reset`, or zero when it asked for none. The depth-stencil surface is a
    /// name for the depth test the page runs, so this is what a guest reading
    /// that surface back with `GetDesc` sees.
    pub depth_format: u32,
    /// The last FVF `SetVertexShader` set. Direct3D 8 describes the vertex
    /// layout this way and has no separate input layout, so this is the whole
    /// description of what a draw's vertices look like.
    pub fvf: u32,
    pub states: HashMap<u32, u32>,
    /// `D3DTSS_*` states, keyed by stage and state.
    pub stage_states: HashMap<(u32, u32), u32>,
    /// `D3DTS_*` transforms, row-major.
    pub transforms: HashMap<u32, [f32; 16]>,
    pub streams: HashMap<u32, Source>,
    pub indices: Option<u32>,
    pub index_base: u32,
    /// Bound textures by stage.
    pub textures: HashMap<u32, u32>,
    pub viewport: (u32, u32, u32, u32, f32, f32),
    /// The shader object `SetVertexShader` bound, when what it was given was a
    /// shader and not an FVF. Zero means the fixed-function layout is in `fvf`.
    pub vertex_shader: u32,
    /// The pixel shader `SetPixelShader` bound, or zero.
    pub pixel_shader: u32,
}

impl Device {
    fn new(width: u32, height: u32, bits: u32, depth_format: u32) -> Self {
        Self {
            width,
            height,
            bits,
            depth_format,
            fvf: 0,
            states: HashMap::new(),
            stage_states: HashMap::new(),
            transforms: HashMap::new(),
            streams: HashMap::new(),
            indices: None,
            index_base: 0,
            textures: HashMap::new(),
            viewport: (0, 0, width, height, 0.0, 1.0),
            vertex_shader: 0,
            pixel_shader: 0,
        }
    }

    /// How many vertices a draw of `primitives` primitives needs, by topology.
    pub fn vertices(topology: u32, primitives: u32) -> Option<u32> {
        match topology {
            D3DPT_POINTLIST => Some(primitives),
            D3DPT_LINELIST => primitives.checked_mul(2),
            D3DPT_LINESTRIP => primitives.checked_add(1),
            D3DPT_TRIANGLELIST => primitives.checked_mul(3),
            D3DPT_TRIANGLESTRIP | D3DPT_TRIANGLEFAN => primitives.checked_add(2),
            _ => None,
        }
    }
}

/// Everything Direct3D 8 keeps between calls: the objects it handed out, and the
/// command stream the page drains.
#[derive(Default)]
pub struct Direct3D {
    /// The `IDirect3D8` object, which `GetDirect3D` answers with.
    pub parent: u32,
    pub device: Option<Device>,
    /// The object `CreateDevice` returned, so a second device is caught rather
    /// than quietly replacing the first.
    pub device_object: u32,
    pub stream: crate::render::Stream,
    pub vertex_buffers: HashMap<u32, Buffer>,
    pub index_buffers: HashMap<u32, Buffer>,
    pub textures: HashMap<u32, Texture>,
    /// Draws the guest issued, which is the number a diagnose report wants.
    pub draws: u64,
    /// Frames presented through `Present`.
    pub frames: u64,
    /// Whether the lighting notice has been printed: a game sets lights on every
    /// frame, and one line is enough to say they are not applied yet.
    pub said_lighting: bool,
    /// The surface object `GetRenderTarget` and `GetBackBuffer` answer with. One
    /// buffer exists here and the page presents it, so the object is a name for
    /// that buffer rather than pixels of its own.
    pub back_buffer_surface: u32,
    /// The render target the guest last bound, or zero for the back buffer.
    pub render_target: u32,
    /// The depth-stencil surface an auto-depth device owns. A guest that asked
    /// for one at `CreateDevice` may read it back and release it, so the answer
    /// is a real object rather than `NULL`.
    pub depth_surface: u32,
    /// The shader objects `CreateVertexShader` and `CreatePixelShader` handed
    /// out. Their bytecode is not executed yet, but the handles are real: a guest
    /// binds one with `SetVertexShader`, reads it back, and releases it.
    pub shaders: std::collections::HashSet<u32>,
    /// Whether the shader notice has been printed. A game creates shaders on
    /// every level, and one line says the picture is fixed-function.
    pub said_shaders: bool,
    /// Whether the notice about a clamped, impossible back-buffer size has been
    /// printed.
    pub said_mode: bool,
    /// Whether the notice about a `CopyRects` that moves no guest bytes has been
    /// printed. RenderWare copies a rendered frame into a texture that way, and
    /// when the source is a surface the page owns there are no bytes here to
    /// copy, which is worth saying once rather than every frame.
    pub said_copy_rects: bool,
    /// Surfaces the device handed out that are not the back buffer: render
    /// targets, depth-stencils and plain images, by their size, format, pool
    /// and usage.
    pub surfaces: HashMap<u32, Surface>,
    /// The level surfaces `GetSurfaceLevel` handed out, by surface object and
    /// the texture level they are a window onto. A guest that fills textures
    /// through their surfaces locks these, and the lock is the texture's own
    /// bytes.
    pub texture_surfaces: HashMap<u32, (u32, u32)>,
}

/// `sizeof(D3DADAPTER_IDENTIFIER8)`: two 512-byte strings, a version, four ids, a
/// GUID and the certification level.
const D3DADAPTER_IDENTIFIER8_BYTES: u32 = 1068;
/// `sizeof(D3DMATERIAL8)`: diffuse, ambient, specular and emissive colours, then
/// specular power. Written so a guest reading back a material sees zeros rather
/// than its own stack.
const D3DMATERIAL8_BYTES: u32 = 68;
/// `sizeof(D3DLIGHT8)`.
const D3DLIGHT8_BYTES: u32 = 104;

/// Zeroes `bytes` at `address`, for a getter whose object exists but has no
/// contents yet.
fn zero(call: &mut Call<'_>, address: u32, bytes: u32) -> Result<()> {
    if address == 0 {
        return Ok(());
    }
    for offset in 0..bytes {
        call.memory.write_u8(address + offset, 0)?;
    }
    Ok(())
}

/// `D3DPRIMITIVETYPE`.
pub const D3DPT_POINTLIST: u32 = 1;
pub const D3DPT_LINELIST: u32 = 2;
pub const D3DPT_LINESTRIP: u32 = 3;
pub const D3DPT_TRIANGLELIST: u32 = 4;
pub const D3DPT_TRIANGLESTRIP: u32 = 5;
pub const D3DPT_TRIANGLEFAN: u32 = 6;

/// `D3DFMT_*`, the formats this host can turn into pixels.
pub const D3DFMT_A8R8G8B8: u32 = 21;
pub const D3DFMT_X8R8G8B8: u32 = 22;
pub const D3DFMT_R5G6B5: u32 = 23;
pub const D3DFMT_X1R5G5B5: u32 = 24;
pub const D3DFMT_A1R5G5B5: u32 = 25;
pub const D3DFMT_A4R4G4B4: u32 = 26;
pub const D3DFMT_A8: u32 = 28;
pub const D3DFMT_P8: u32 = 41;
pub const D3DFMT_L8: u32 = 50;
/// The depth-stencil formats, which have no colour to convert and are only
/// named so `GetDesc` can report them and their byte depth.
pub const D3DFMT_D32: u32 = 71;
pub const D3DFMT_D24S8: u32 = 75;
pub const D3DFMT_D16: u32 = 80;
pub const D3DFMT_INDEX16: u32 = 101;

/// `D3DRTYPE_*`, the resource kinds a `D3DSURFACE_DESC` names. A surface is
/// always a surface.
const D3DRTYPE_SURFACE: u32 = 1;

/// `D3DPOOL_DEFAULT`, the pool the device's own surfaces live in.
const D3DPOOL_DEFAULT: u32 = 0;
/// `D3DPOOL_SYSTEMMEM`, the only pool an image surface can be in.
const D3DPOOL_SYSTEMMEM: u32 = 2;

/// `D3DUSAGE_*` for the two surface kinds the device draws into.
const D3DUSAGE_RENDERTARGET: u32 = 1;
const D3DUSAGE_DEPTHSTENCIL: u32 = 2;

/// `D3DCLEAR_*`.
pub const D3DCLEAR_TARGET: u32 = 1;
pub const D3DCLEAR_ZBUFFER: u32 = 2;
pub const D3DCLEAR_STENCIL: u32 = 4;

/// Bytes per pixel of a format, or `None` for one this host does not convert.
fn bytes_per_pixel(format: u32) -> Option<u32> {
    match format {
        D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8 => Some(4),
        D3DFMT_R5G6B5 | D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 | D3DFMT_A4R4G4B4 => Some(2),
        D3DFMT_A8 | D3DFMT_P8 | D3DFMT_L8 => Some(1),
        _ => None,
    }
}

/// The bytes a surface of `format` this size occupies, which is what
/// `D3DSURFACE_DESC.Size` reports and what a guest sizes its copies from. A
/// depth-stencil format has no colour bytes to read, so the two common ones
/// name their own depth and any other format answers zero rather than a guess.
fn surface_bytes(format: u32, width: u32, height: u32) -> u32 {
    let bytes = match format {
        D3DFMT_D24S8 | D3DFMT_D32 => 4,
        D3DFMT_D16 => 2,
        _ => bytes_per_pixel(format).unwrap_or(0),
    };
    width * height * bytes
}

/// Bytes one vertex takes, from the FVF alone.
///
/// This is needed for `DrawPrimitiveUP`, which passes no stride, and for a stream
/// source created with a zero stride. The texture-coordinate sizes come from two
/// bits per coordinate set, and the position from the position mask, because a
/// transformed vertex is a `float4` and an untransformed one a `float3`.
pub fn fvf_stride(fvf: u32) -> Option<u32> {
    let mut stride = match fvf & 0x00e {
        0x002 => 12, // D3DFVF_XYZ
        0x004 => 16, // D3DFVF_XYZRHW
        0x006 => 16, // XYZB1
        0x008 => 20, // XYZB2
        0x00a => 24, // XYZB3
        0x00c => 28, // XYZB4
        0x00e => 32, // XYZB5
        _ => return None,
    };
    if fvf & 0x010 != 0 {
        stride += 12; // D3DFVF_NORMAL
    }
    if fvf & 0x020 != 0 {
        stride += 4; // D3DFVF_PSIZE
    }
    if fvf & 0x040 != 0 {
        stride += 4; // D3DFVF_DIFFUSE
    }
    if fvf & 0x080 != 0 {
        stride += 4; // D3DFVF_SPECULAR
    }
    for set in 0..((fvf >> 8) & 0xf) {
        let size = 2 + ((fvf >> (16 + set * 2)) & 3);
        stride += size * 4;
    }
    Some(stride)
}

/// `IDirect3D8`, in vtable order.
const IDIRECT3D8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "RegisterSoftwareDevice",
    "GetAdapterCount",
    "GetAdapterIdentifier",
    "GetAdapterModeCount",
    "EnumAdapterModes",
    "GetAdapterDisplayMode",
    "CheckDeviceType",
    "CheckDeviceFormat",
    "CheckDeviceMultiSampleType",
    "CheckDepthStencilMatch",
    "GetDeviceCaps",
    "GetAdapterMonitor",
    "CreateDevice",
];

/// `IDirect3DDevice8`: the interface a guest spends its frame in.
const IDIRECT3DDEVICE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "TestCooperativeLevel",
    "GetAvailableTextureMem",
    "ResourceManagerDiscardBytes",
    "GetDirect3D",
    "GetDeviceCaps",
    "GetDisplayMode",
    "GetCreationParameters",
    "SetCursorProperties",
    "SetCursorPosition",
    "ShowCursor",
    "CreateAdditionalSwapChain",
    "Reset",
    "Present",
    "GetBackBuffer",
    "GetRasterStatus",
    "SetGammaRamp",
    "GetGammaRamp",
    "CreateTexture",
    "CreateVolumeTexture",
    "CreateCubeTexture",
    "CreateVertexBuffer",
    "CreateIndexBuffer",
    "CreateRenderTarget",
    "CreateDepthStencilSurface",
    "CreateImageSurface",
    "CopyRects",
    "UpdateTexture",
    "GetFrontBuffer",
    "SetRenderTarget",
    "GetRenderTarget",
    "GetDepthStencilSurface",
    "BeginScene",
    "EndScene",
    "Clear",
    "SetTransform",
    "GetTransform",
    "MultiplyTransform",
    "SetViewport",
    "GetViewport",
    "SetMaterial",
    "GetMaterial",
    "SetLight",
    "GetLight",
    "LightEnable",
    "GetLightEnable",
    "SetClipPlane",
    "GetClipPlane",
    "SetRenderState",
    "GetRenderState",
    "BeginStateBlock",
    "EndStateBlock",
    "ApplyStateBlock",
    "CaptureStateBlock",
    "DeleteStateBlock",
    "CreateStateBlock",
    "SetClipStatus",
    "GetClipStatus",
    "GetTexture",
    "SetTexture",
    "GetTextureStageState",
    "SetTextureStageState",
    "ValidateDevice",
    "GetInfo",
    "SetPaletteEntries",
    "GetPaletteEntries",
    "SetCurrentTexturePalette",
    "GetCurrentTexturePalette",
    "DrawPrimitive",
    "DrawIndexedPrimitive",
    "DrawPrimitiveUP",
    "DrawIndexedPrimitiveUP",
    "ProcessVertices",
    "CreateVertexShader",
    "SetVertexShader",
    "GetVertexShader",
    "DeleteVertexShader",
    "SetVertexShaderConstant",
    "GetVertexShaderConstant",
    "GetVertexShaderDeclaration",
    "GetVertexShaderFunction",
    "SetStreamSource",
    "GetStreamSource",
    "SetIndices",
    "GetIndices",
    "CreatePixelShader",
    "SetPixelShader",
    "GetPixelShader",
    "DeletePixelShader",
    "SetPixelShaderConstant",
    "GetPixelShaderConstant",
    "GetPixelShaderFunction",
    "DrawRectPatch",
    "DrawTriPatch",
    "DeletePatch",
];

/// `IDirect3DResource8`: what every buffer and texture is, before its own slots.
const IDIRECT3DRESOURCE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
];

/// `IDirect3DVertexBuffer8`.
const IDIRECT3DVERTEXBUFFER8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
    "Lock",
    "Unlock",
    "GetDesc",
];

/// `IDirect3DIndexBuffer8`, which is the vertex buffer's shape with a 16- or
/// 32-bit element.
const IDIRECT3DINDEXBUFFER8: &[&str] = IDIRECT3DVERTEXBUFFER8;

/// `IDirect3DBaseTexture8`: the resource slots plus the LOD and mip selection
/// every texture shares.
const IDIRECT3DBASETEXTURE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
    "SetLOD",
    "GetLOD",
    "GetLevelCount",
];

/// `IDirect3DTexture8`.
const IDIRECT3DTEXTURE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
    "SetLOD",
    "GetLOD",
    "GetLevelCount",
    "GetLevelDesc",
    "GetSurfaceLevel",
    "LockRect",
    "UnlockRect",
    "AddDirtyRect",
];

/// `IDirect3DVolumeTexture8`: a texture with a depth, so its levels are boxes.
const IDIRECT3DVOLUMETEXTURE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
    "SetLOD",
    "GetLOD",
    "GetLevelCount",
    "GetLevelDesc",
    "GetVolumeLevel",
    "LockBox",
    "UnlockBox",
    "AddDirtyBox",
];

/// `IDirect3DCubeTexture8`: six faces of levels, addressed by face as well.
const IDIRECT3DCUBETEXTURE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "SetPriority",
    "GetPriority",
    "PreLoad",
    "GetType",
    "SetLOD",
    "GetLOD",
    "GetLevelCount",
    "GetLevelDesc",
    "GetCubeMapSurface",
    "LockRect",
    "UnlockRect",
    "AddDirtyRect",
];

/// `IDirect3DSurface8`.
const IDIRECT3DSURFACE8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "GetContainer",
    "GetDesc",
    "LockRect",
    "UnlockRect",
];

/// `IDirect3DVolume8`.
const IDIRECT3DVOLUME8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "GetDevice",
    "SetPrivateData",
    "GetPrivateData",
    "FreePrivateData",
    "GetContainer",
    "GetDesc",
    "LockBox",
    "UnlockBox",
];

/// `IDirect3DSwapChain8`.
const IDIRECT3DSWAPCHAIN8: &[&str] = &[
    "QueryInterface",
    "AddRef",
    "Release",
    "Present",
    "GetBackBuffer",
];

/// Argument bytes per vtable slot, `this` included, parsed from `d3d8.h`.
///
/// Every D3D8 parameter is four bytes in the x86 ABI -- a pointer, a `DWORD`, a
/// `UINT`, an enum or a `float` -- so this is the header's parameter count plus
/// `this`, which the ABI pushes too. It is a table rather than a `match` because
/// a wrong pop corrupts the guest's stack: the number has to come from the
/// declaration, not from what a shim happens to read.
const ID3D8_ARGS: &[u32] = &[
    // Every one of these is the d3d8.h parameter count plus `this`, and the
    // pop is what the guest's stack depends on. `EnumAdapterModes` is the one
    // that differs between versions: D3D8 declares `(Adapter, Mode, pMode)`,
    // while D3D9 added the `Format` filter in front of `Mode`. A guest
    // enumerating modes pushes three arguments, so sixteen bytes, not twenty.
    12, 4, 4, 8, 4, 16, 8, 16, 12, 24, 28, 24, 24, 16, 8, 28,
];
const ID3DDEVICE8_ARGS: &[u32] = &[
    12, 4, 4, 4, 4, 8, 8, 8, 8, 8, 16, 16, 8, 12, 8, 20, 16, 8, 12, 8, 32, 36, 28, 24, 24, 28, 24,
    20, 24, 12, 8, 12, 8, 8, 4, 4, 28, 12, 12, 12, 8, 8, 8, 8, 12, 12, 12, 12, 12, 12, 12, 12, 4,
    8, 8, 8, 8, 12, 8, 8, 12, 12, 16, 16, 8, 16, 12, 12, 8, 8, 16, 24, 20, 36, 24, 20, 8, 8, 8, 16,
    16, 16, 16, 16, 16, 12, 12, 12, 8, 8, 8, 16, 16, 16, 16, 16, 8,
];
const ID3DRESOURCE8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4];
const ID3DVERTEXBUFFER8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 20, 4, 8];
const ID3DINDEXBUFFER8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 20, 4, 8];
const ID3DBASETEXTURE8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 8, 4, 4];
const ID3DTEXTURE8_ARGS: &[u32] = &[
    12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 8, 4, 4, 12, 12, 20, 8, 8,
];
const ID3DVOLUMETEXTURE8_ARGS: &[u32] = &[
    12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 8, 4, 4, 12, 12, 20, 8, 8,
];
const ID3DCUBETEXTURE8_ARGS: &[u32] = &[
    12, 4, 4, 8, 20, 16, 8, 8, 4, 4, 4, 8, 4, 4, 12, 16, 24, 12, 12,
];
const ID3DSURFACE8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 12, 8, 16, 4];
const ID3DVOLUME8_ARGS: &[u32] = &[12, 4, 4, 8, 20, 16, 8, 12, 8, 16, 4];
const ID3DSWAPCHAIN8_ARGS: &[u32] = &[12, 4, 4, 20, 16];

/// Every interface this module models: its thunk kind, its name, its vtable, and
/// the bytes each of those methods pops.
const INTERFACES: &[(u8, &str, &[&str], &[u32])] = &[
    (D3D8, "IDirect3D8", IDIRECT3D8, ID3D8_ARGS),
    (D3D8_DEVICE, "IDirect3DDevice8", IDIRECT3DDEVICE8, ID3DDEVICE8_ARGS),
    (D3D8_RESOURCE, "IDirect3DResource8", IDIRECT3DRESOURCE8, ID3DRESOURCE8_ARGS),
    (
        D3D8_VERTEX_BUFFER,
        "IDirect3DVertexBuffer8",
        IDIRECT3DVERTEXBUFFER8,
        ID3DVERTEXBUFFER8_ARGS,
    ),
    (
        D3D8_INDEX_BUFFER,
        "IDirect3DIndexBuffer8",
        IDIRECT3DINDEXBUFFER8,
        ID3DINDEXBUFFER8_ARGS,
    ),
    (
        D3D8_BASE_TEXTURE,
        "IDirect3DBaseTexture8",
        IDIRECT3DBASETEXTURE8,
        ID3DBASETEXTURE8_ARGS,
    ),
    (
        D3D8_TEXTURE,
        "IDirect3DTexture8",
        IDIRECT3DTEXTURE8,
        ID3DTEXTURE8_ARGS,
    ),
    (
        D3D8_VOLUME_TEXTURE,
        "IDirect3DVolumeTexture8",
        IDIRECT3DVOLUMETEXTURE8,
        ID3DVOLUMETEXTURE8_ARGS,
    ),
    (
        D3D8_CUBE_TEXTURE,
        "IDirect3DCubeTexture8",
        IDIRECT3DCUBETEXTURE8,
        ID3DCUBETEXTURE8_ARGS,
    ),
    (D3D8_SURFACE, "IDirect3DSurface8", IDIRECT3DSURFACE8, ID3DSURFACE8_ARGS),
    (D3D8_VOLUME, "IDirect3DVolume8", IDIRECT3DVOLUME8, ID3DVOLUME8_ARGS),
    (
        D3D8_SWAP_CHAIN,
        "IDirect3DSwapChain8",
        IDIRECT3DSWAPCHAIN8,
        ID3DSWAPCHAIN8_ARGS,
    ),
];

/// Whether `kind` is one of this module's interfaces.
pub fn handles(kind: u8) -> bool {
    INTERFACES.iter().any(|entry| entry.0 == kind)
}

fn interface(kind: u8) -> Option<(&'static str, &'static [&'static str], &'static [u32])> {
    INTERFACES
        .iter()
        .find(|entry| entry.0 == kind)
        .map(|entry| (entry.1, entry.2, entry.3))
}

/// The names of one interface's vtable, for the tests and for diagnostics.
pub fn names(kind: u8) -> Option<&'static [&'static str]> {
    interface(kind).map(|(_, table, _)| table)
}

/// Argument bytes a method pops, `this` included.
///
/// This is the count from `d3d8.h`, so it is right for every slot, not only the
/// ones answered today: the pop is what the caller's stack depends on, and a
/// refused method never gets that far only because the refusal stops the session
/// first. A slot past the vtable, or a kind that is not a D3D8 interface, gets
/// one dword -- a caller there is already wrong.
pub fn arg_bytes(kind: u8, slot: u8) -> u32 {
    match interface(kind) {
        Some((_, _, args)) => args.get(usize::from(slot)).copied().unwrap_or(4),
        None => 4,
    }
}

/// `Interface.Method` for a call, or `Interface.slot n` for one past the vtable.
pub fn label(kind: u8, slot: u8) -> String {
    match interface(kind) {
        Some((name, table, _)) => match table.get(usize::from(slot)) {
            Some(method) => format!("{name}.{method}"),
            None => format!("{name}.slot {slot}"),
        },
        None => format!("kind {kind}.slot {slot}"),
    }
}

/// Answers one method call: the reference-count trio, then the method's own
/// implementation. Everything this device does not do yet is a named refusal.
pub fn serve(host: &mut Host, call: &mut Call<'_>, kind: u8, slot: u8) -> Result<Action> {
    match slot {
        0 => query_interface(call),
        1 => Ok(Return(1)),
        2 => release(host, call, kind),
        _ => {
            let Some(method) = names(kind).and_then(|table| table.get(usize::from(slot)).copied())
            else {
                return Err(refusal(kind, slot));
            };
            match kind {
                D3D8 => adapter(method, host, call),
                D3D8_DEVICE => device(method, host, call),
                D3D8_VERTEX_BUFFER => buffer(method, host, call, false),
                D3D8_INDEX_BUFFER => buffer(method, host, call, true),
                D3D8_TEXTURE => texture(method, host, call),
                D3D8_SURFACE => surface(method, host, call),
                D3D8_SWAP_CHAIN => swap_chain(method, host, call),
                _ => Err(refusal(kind, slot)),
            }
        }
    }
}

/// The `n`-th declared parameter of a COM call.
///
/// A method's first stack argument is `this`, which the session leaves at `args`
/// exactly as it does for an import, so a shim's own parameters start one dword
/// later. Getting this wrong does not look like an argument bug: `CreateDevice`
/// then reads `BehaviorFlags` where `pPresentationParameters` should be and
/// faults on an address of 0x20.
fn parameter(call: &Call<'_>, index: u32) -> Result<u32> {
    call.arg(index + 1)
}

fn refusal(kind: u8, slot: u8) -> String {
    format!(
        "{} has no shim: the object exists but this method does",
        label(kind, slot)
    )
}

/// `Release`: the last reference to a resource frees the guest memory behind it
/// and tells the page to drop its copy.
///
/// Direct3D resources really are created and destroyed per level, so dropping the
/// bytes here is what keeps a session from walking the arena to its end. The
/// object itself is four bytes; it goes back too.
///
/// The count is not tracked. Nothing else in this host hands out two objects that
/// share a resource's memory, so the first `Release` is the last.
fn release(host: &mut Host, call: &mut Call<'_>, kind: u8) -> Result<Action> {
    let object = call.arg(0)?;
    if kind == D3D8_SURFACE {
        host.d3d8.texture_surfaces.remove(&object);
        host.d3d8.surfaces.remove(&object);
        // The back buffer and depth-stencil names belong to the page's own
        // buffers, and a guest saves and restores them without creating them.
        if object != host.d3d8.back_buffer_surface && object != host.d3d8.depth_surface {
            host.release(object);
        }
        return Ok(Return(1));
    }
    let freed = match kind {
        D3D8_VERTEX_BUFFER => host.d3d8.vertex_buffers.remove(&object).map(|b| b.address),
        D3D8_INDEX_BUFFER => host.d3d8.index_buffers.remove(&object).map(|b| b.address),
        D3D8_TEXTURE => host.d3d8.textures.remove(&object).map(|t| t.address),
        _ => None,
    };
    if let Some(address) = freed {
        host.release(address);
        host.release(object);
        host.d3d8.stream.begin(op::DESTROY).u32(object);
    }
    Ok(Return(1))
}

/// The `IDirect3D8` methods. This interface only ever answers questions about
/// the adapter, so its answers describe the one device this host provides.
fn adapter(method: &str, host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    match method {
        "GetAdapterCount" => Ok(Return(1)),
        // One mode exists: the one the page is drawing. A guest that enumerates
        // the list to pick a resolution finds exactly what it is already running
        // at, and asking for the one past the end is how the list ends.
        "GetAdapterModeCount" => Ok(Return(1)),
        "EnumAdapterModes" => {
            // `EnumAdapterModes(Adapter, Mode, pMode)`, the count above answering
            // for every index: one mode exists, the one the page is drawing.
            let index = parameter(call, 1)?;
            let destination = parameter(call, 2)?;
            if index != 0 || destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let (width, height) = (host.ddraw.width(), host.ddraw.height());
            // D3D8 filters by no format here, so the answer is the 32-bit
            // framebuffer this host draws in.
            let format = D3DFMT_X8R8G8B8;
            call.write_u32(destination, width)?;
            call.write_u32(destination + 4, height)?;
            call.write_u32(destination + 8, 60)?;
            call.write_u32(destination + 12, format)?;
            Ok(Return(S_OK))
        }
        "GetAdapterDisplayMode" => {
            let destination = parameter(call, 1)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let (width, height) = (host.ddraw.width(), host.ddraw.height());
            call.write_u32(destination, width)?;
            call.write_u32(destination + 4, height)?;
            call.write_u32(destination + 8, 60)?;
            call.write_u32(destination + 12, D3DFMT_X8R8G8B8)?;
            Ok(Return(S_OK))
        }
        // A guest reads the driver strings for a log and a GUID to pick an
        // adapter; it does not branch on either, so the answers are left alone
        // rather than guessed at. `GetAdapterIdentifier` zeroes the string on a
        // real machine, and a zeroed buffer reads as an empty name.
        "GetAdapterIdentifier" => {
            // `GetAdapterIdentifier(Adapter, Flags, pIdentifier)`: the flags are
            // the second parameter and the structure is the third.
            zero(call, parameter(call, 2)?, D3DADAPTER_IDENTIFIER8_BYTES)?;
            Ok(Return(S_OK))
        }
        "GetAdapterMonitor" => Ok(Return(0)),
        // Whether a mode is supported is a question this host answers by
        // accepting it: the page's canvas scales whatever the guest asks for.
        "CheckDeviceType" | "CheckDeviceFormat" | "CheckDeviceMultiSampleType"
        | "CheckDepthStencilMatch" => Ok(Return(S_OK)),
        // `GetDeviceCaps(Adapter, DeviceType, pCaps)`: the caps are the third
        // argument here and the first on the device, which is why the writer is
        // told where to look rather than assuming.
        "GetDeviceCaps" => device_caps(host, call, 2),
        "CreateDevice" => create_device(host, call),
        _ => Err(format!(
            "IDirect3D8.{method} has no shim: the object exists but this method does"
        )),
    }
}

/// `CreateDevice(Adapter, DeviceType, hFocusWindow, BehaviorFlags,
/// pPresentationParameters, ppReturnedDeviceInterface)`.
///
/// The presentation parameters say how big the back buffer is, which is the size
/// the page has to give its drawing buffer, so that is read and recorded. The
/// device is created even when the guest asks for a depth buffer this host does
/// not provide: the depth test still runs in the page against a depth buffer the
/// page allocates.
fn create_device(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parameters = parameter(call, 4)?;
    let destination = parameter(call, 5)?;
    if parameters == 0 || destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    let (width, height, bits, depth_format) = presentation(host, call, parameters)?;
    let asked = (
        call.memory.read_u32(parameters)?,
        call.memory.read_u32(parameters + 4)?,
    );
    if impossible(asked.0, asked.1) && !host.d3d8.said_mode {
        // A guest that reads a resolution out of a table it never filled gets
        // here. The answer is still a device -- the page's canvas is the size of
        // the window, and a game that boots is worth more than one that exits on
        // a mode no display has -- but saying so is what keeps a clamped size
        // from being mistaken for the mode the guest asked for.
        host.d3d8.said_mode = true;
        host.notice(format!(
            "Direct3D 8: CreateDevice asked for {}x{}, which no display has; \
             this back buffer is the window's {width}x{height}",
            asked.0, asked.1
        ));
    }
    if host.d3d8.device_object != 0 {
        return Err(
            "Direct3D 8: CreateDevice called a second time; this host provides one device"
                .to_string(),
        );
    }
    let object = thunks::new_object(call.memory, host, D3D8_DEVICE)?;
    host.d3d8.device = Some(Device::new(width, height, bits, depth_format));
    host.d3d8.device_object = object;
    host.d3d8
        .stream
        .begin(op::CREATE_DEVICE)
        .u32(width)
        .u32(height)
        .u32(bits);
    call.write_u32(destination, object)?;
    host.notice(format!("Direct3D 8: device created {width}x{height}x{bits}"));
    // A full-screen device takes the display over, and the runtime shows the
    // focus window and hands it the keyboard: the `WM_ACTIVATE` a game's message
    // loop waits for before it runs a frame. A windowed one leaves the window
    // as its owner made it.
    let windowed = call.memory.read_u32(parameters + 28).unwrap_or(1);
    if windowed == 0 {
        let focus = parameter(call, 2)?;
        let window = if focus != 0 {
            focus
        } else {
            call.memory.read_u32(parameters + 24).unwrap_or(0)
        };
        crate::shim::user32::activate(host, window);
    }
    Ok(Return(S_OK))
}

/// What a `D3DPRESENT_PARAMETERS` describes: the back buffer's size and colour
/// depth, with a zero size meaning "the window's", and the format of the auto
/// depth-stencil the guest asked for, with zero meaning it asked for none.
fn presentation(host: &Host, call: &Call<'_>, parameters: u32) -> Result<(u32, u32, u32, u32)> {
    let width = call.memory.read_u32(parameters)?;
    let height = call.memory.read_u32(parameters + 4)?;
    let format = call.memory.read_u32(parameters + 8)?;
    // A window-mode device leaves the size zero and the window decides; a size no
    // display has is not a mode this host can honour either, so the window decides
    // there too and the caller says so.
    let (width, height) = if width == 0 || height == 0 || impossible(width, height) {
        (host.ddraw.width(), host.ddraw.height())
    } else {
        (width, height)
    };
    let bits = bytes_per_pixel(format).map_or(32, |bytes| bytes * 8);
    let auto_depth = call.memory.read_u32(parameters + 32)?;
    let depth_format = if auto_depth != 0 {
        call.memory.read_u32(parameters + 36)?
    } else {
        0
    };
    Ok((width, height, bits, depth_format))
}

/// Whether a requested back buffer is a mode no display has.
fn impossible(width: u32, height: u32) -> bool {
    const LARGEST_MODE: u32 = 16_384;
    width > LARGEST_MODE || height > LARGEST_MODE
}

/// A `D3DPRESENT_PARAMETERS` is 15 dwords and then two pointers, but `Reset`
/// only reads the size, which is why the rest is ignored.
fn reset(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parameters = parameter(call, 0)?;
    if parameters == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    let (width, height, bits, depth_format) = presentation(host, call, parameters)?;
    let Some(device) = host.d3d8.device.as_mut() else {
        return Ok(Return(D3DERR_INVALIDCALL));
    };
    device.width = width;
    device.height = height;
    device.bits = bits;
    device.depth_format = depth_format;
    device.viewport = (0, 0, width, height, 0.0, 1.0);
    host.d3d8
        .stream
        .begin(op::CREATE_DEVICE)
        .u32(width)
        .u32(height)
        .u32(bits);
    Ok(Return(S_OK))
}

/// Says once that shader bytecode is accepted but not run, so a picture drawn
/// beside a shader is understood rather than mistaken for a supported one.
fn shader_notice(host: &mut Host) {
    if !host.d3d8.said_shaders {
        host.d3d8.said_shaders = true;
        host.notice(
            "Direct3D 8: shaders are created and bound but not executed yet; geometry is"
                .to_string()
                + " drawn through the fixed-function path",
        );
    }
}

/// The object that names the buffer the page presents, made once per session.
fn back_buffer(host: &mut Host, call: &mut Call<'_>) -> Result<u32> {
    if host.d3d8.back_buffer_surface == 0 {
        host.d3d8.back_buffer_surface = thunks::new_object(call.memory, host, D3D8_SURFACE)?;
    }
    Ok(host.d3d8.back_buffer_surface)
}

/// The device's state, or a stop when a device method runs before
/// `CreateDevice` answered.
fn state(host: &Host) -> Result<&Device> {
    host.d3d8
        .device
        .as_ref()
        .ok_or_else(|| "Direct3D 8: a device method ran with no device".to_string())
}

/// The `IDirect3DDevice8` methods.
///
/// Every answered method either changes the state the page mirrors or writes one
/// record. The two are not separable: a draw reads the state that was set before
/// it in the same stream, so a method that stores without recording would draw
/// with the wrong matrix.
fn device(method: &str, host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // A device method before the device exists is the guest's own ordering bug on
    // Windows, where it would fault on a NULL interface; naming the method here
    // is what says which call the guest thought it had made.
    if host.d3d8.device.is_none() {
        return Err(format!(
            "Direct3D 8: {method} ran before CreateDevice answered"
        ));
    }
    match method {
        "TestCooperativeLevel" => Ok(Return(S_OK)),
        "GetAvailableTextureMem" => Ok(Return(64 << 20)),
        "GetDirect3D" => {
            let destination = parameter(call, 0)?;
            if destination != 0 {
                call.write_u32(destination, host.d3d8.parent)?;
            }
            Ok(Return(S_OK))
        }
        "GetDeviceCaps" => device_caps(host, call, 0),
        "GetCreationParameters" => {
            let destination = parameter(call, 0)?;
            if destination != 0 {
                call.write_u32(destination, 0)?; // AdapterOrdinal
                call.write_u32(destination + 4, 1)?; // D3DDEVTYPE_HAL
                call.write_u32(destination + 8, 0)?; // hFocusWindow
                call.write_u32(destination + 12, 0x22)?; // FPU_PRESERVE | SOFTWARE_VERTEXPROCESSING
            }
            Ok(Return(S_OK))
        }
        "GetDisplayMode" => {
            let destination = parameter(call, 0)?;
            let device = state(host)?;
            let (width, height) = (device.width, device.height);
            if destination != 0 {
                call.write_u32(destination, width)?;
                call.write_u32(destination + 4, height)?;
                call.write_u32(destination + 8, 60)?;
                call.write_u32(destination + 12, D3DFMT_X8R8G8B8)?;
            }
            Ok(Return(S_OK))
        }
        "GetRasterStatus" => {
            let destination = parameter(call, 0)?;
            if destination != 0 {
                call.write_u32(destination, 0)?;
                call.write_u32(destination + 4, 0)?;
            }
            Ok(Return(S_OK))
        }
        "SetRenderState" => {
            let key = parameter(call, 0)?;
            let value = parameter(call, 1)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetRenderState with no device".to_string())?;
            device.states.insert(key, value);
            host.d3d8
                .stream
                .begin(op::SET_RENDER_STATE)
                .u32(key)
                .u32(value);
            Ok(Return(S_OK))
        }
        "GetRenderState" => {
            let key = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            let value = state(host)?.states.get(&key).copied().unwrap_or(0);
            if destination != 0 {
                call.write_u32(destination, value)?;
            }
            Ok(Return(S_OK))
        }
        "SetTextureStageState" => {
            let stage = parameter(call, 0)?;
            let key = parameter(call, 1)?;
            let value = parameter(call, 2)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetTextureStageState with no device".to_string())?;
            device.stage_states.insert((stage, key), value);
            host.d3d8
                .stream
                .begin(op::SET_TEXTURE_STAGE_STATE)
                .u32(stage)
                .u32(key)
                .u32(value);
            Ok(Return(S_OK))
        }
        "GetTextureStageState" => {
            let stage = parameter(call, 0)?;
            let key = parameter(call, 1)?;
            let destination = parameter(call, 2)?;
            let value = state(host)?
                .stage_states
                .get(&(stage, key))
                .copied()
                .unwrap_or(0);
            if destination != 0 {
                call.write_u32(destination, value)?;
            }
            Ok(Return(S_OK))
        }
        "SetTransform" => {
            let key = parameter(call, 0)?;
            let source = parameter(call, 1)?;
            if source == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let matrix = read_matrix(call.memory, source)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetTransform with no device".to_string())?;
            device.transforms.insert(key, matrix);
            let mut record = host.d3d8.stream.begin(op::SET_TRANSFORM);
            record.u32(key).matrix(&matrix);
            Ok(Return(S_OK))
        }
        "GetTransform" => {
            let key = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            let identity = IDENTITY;
            let matrix = state(host)?
                .transforms
                .get(&key)
                .copied()
                .unwrap_or(identity);
            if destination != 0 {
                write_matrix(call, destination, &matrix)?;
            }
            Ok(Return(S_OK))
        }
        "MultiplyTransform" => {
            let key = parameter(call, 0)?;
            let source = parameter(call, 1)?;
            if source == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let right = read_matrix(call.memory, source)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: MultiplyTransform with no device".to_string())?;
            let left = device.transforms.get(&key).copied().unwrap_or(IDENTITY);
            let matrix = multiply(&left, &right);
            device.transforms.insert(key, matrix);
            let mut record = host.d3d8.stream.begin(op::SET_TRANSFORM);
            record.u32(key).matrix(&matrix);
            Ok(Return(S_OK))
        }
        "SetViewport" => {
            let source = parameter(call, 0)?;
            if source == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let viewport = (
                call.memory.read_u32(source)?,
                call.memory.read_u32(source + 4)?,
                call.memory.read_u32(source + 8)?,
                call.memory.read_u32(source + 12)?,
                f32::from_bits(call.memory.read_u32(source + 16)?),
                f32::from_bits(call.memory.read_u32(source + 20)?),
            );
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetViewport with no device".to_string())?;
            device.viewport = viewport;
            let mut record = host.d3d8.stream.begin(op::SET_VIEWPORT);
            record
                .u32(viewport.0)
                .u32(viewport.1)
                .u32(viewport.2)
                .u32(viewport.3)
                .f32(viewport.4)
                .f32(viewport.5);
            Ok(Return(S_OK))
        }
        "GetViewport" => {
            let destination = parameter(call, 0)?;
            let viewport = state(host)?.viewport;
            if destination != 0 {
                call.write_u32(destination, viewport.0)?;
                call.write_u32(destination + 4, viewport.1)?;
                call.write_u32(destination + 8, viewport.2)?;
                call.write_u32(destination + 12, viewport.3)?;
                call.write_u32(destination + 16, viewport.4.to_bits())?;
                call.write_u32(destination + 20, viewport.5.to_bits())?;
            }
            Ok(Return(S_OK))
        }
        "Clear" => {
            let flags = parameter(call, 2)?;
            let colour = parameter(call, 3)?;
            let depth = f32::from_bits(parameter(call, 4)?);
            let stencil = parameter(call, 5)?;
            state(host)?;
            host.d3d8
                .stream
                .begin(op::CLEAR)
                .u32(flags)
                .u32(colour)
                .f32(depth)
                .u32(stencil);
            Ok(Return(S_OK))
        }
        // A scene is a hint to the driver, not a state change: nothing between
        // `BeginScene` and `EndScene` draws differently because of it.
        "BeginScene" | "EndScene" => Ok(Return(S_OK)),
        // The device has one buffer, and the page presents it. These answer with
        // that buffer so the guest can save the target it is drawing into and put
        // it back, which is what a 2005 game's menu code does around a full-screen
        // effect.
        "GetRenderTarget" | "GetFrontBuffer" | "GetBackBuffer" => {
            state(host)?;
            let destination = if method == "GetRenderTarget" {
                parameter(call, 0)?
            } else {
                // `GetBackBuffer(BackBuffer, Type, ppBackBuffer)` and
                // `GetFrontBuffer(ppFrontBuffer)` both hand back the one buffer.
                parameter(call, if method == "GetBackBuffer" { 2 } else { 0 })?
            };
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let surface = back_buffer(host, call)?;
            call.write_u32(destination, surface)?;
            Ok(Return(S_OK))
        }
        // `SetRenderTarget(pRenderTarget, pNewZStencil)`: both arguments are
        // surface handles, not the out-pointers their names suggest -- there is
        // no `GetRenderTarget`-style write here, and treating the first one as
        // one overwrites a surface object with a pointer to itself.
        "SetRenderTarget" => {
            state(host)?;
            let wanted = parameter(call, 0)?;
            let depth = parameter(call, 1)?;
            // Only the buffer this host has can be drawn into; anything else is
            // refused by name rather than silently redirected, because a picture
            // drawn into the wrong target is worse than a stop that says so.
            if wanted != 0 && wanted != host.d3d8.back_buffer_surface {
                return Err(format!(
                    "Direct3D 8: SetRenderTarget asked for a surface that is not the back buffer\
                     ({wanted:#x}, this device's buffer is {:#x}, its depth is {depth:#x}); this \
                     host draws into one buffer",
                    host.d3d8.back_buffer_surface
                ));
            }
            host.d3d8.render_target = wanted;
            Ok(Return(S_OK))
        }
        "GetDepthStencilSurface" => {
            state(host)?;
            let destination = parameter(call, 0)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            if host.d3d8.depth_surface == 0 {
                host.d3d8.depth_surface = thunks::new_object(call.memory, host, D3D8_SURFACE)?;
            }
            let surface = host.d3d8.depth_surface;
            call.write_u32(destination, surface)?;
            Ok(Return(S_OK))
        }
        "Present" => {
            state(host)?;
            host.d3d8.stream.begin(op::PRESENT);
            host.d3d8.frames += 1;
            // The flip keeps `wh_flips`-style pacing honest for a page that draws
            // through DirectDraw and Direct3D at once.
            host.ddraw.present();
            Ok(Return(S_OK))
        }
        "Reset" => reset(host, call),
        // A surface this host cannot draw into is still a resource the guest
        // allocates, sizes, and releases; handing it back with its real size lets
        // that bookkeeping run, and a draw into it is refused where it happens.
        "CreateRenderTarget" | "CreateDepthStencilSurface" | "CreateImageSurface" => {
            state(host)?;
            // All three start `(Width, Height, Format)` and end with the pointer
            // that receives the surface, but what sits between differs, and the
            // destination is where the guest's stack depends on the pop being
            // right: `CreateRenderTarget` has a lockable flag after its
            // multi-sample, `CreateDepthStencilSurface` stops at the
            // multi-sample, and `CreateImageSurface` has neither.
            let width = parameter(call, 0)?;
            let height = parameter(call, 1)?;
            let format = parameter(call, 2)?;
            let (pool, usage, destination) = match method {
                "CreateRenderTarget" => (D3DPOOL_DEFAULT, D3DUSAGE_RENDERTARGET, parameter(call, 5)?),
                "CreateDepthStencilSurface" => {
                    (D3DPOOL_DEFAULT, D3DUSAGE_DEPTHSTENCIL, parameter(call, 4)?)
                }
                _ => (D3DPOOL_SYSTEMMEM, 0, parameter(call, 3)?),
            };
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let surface = thunks::new_object(call.memory, host, D3D8_SURFACE)?;
            host.d3d8.surfaces.insert(
                surface,
                Surface {
                    width,
                    height,
                    format,
                    pool,
                    usage,
                },
            );
            call.write_u32(destination, surface)?;
            Ok(Return(S_OK))
        }
        "CreateVertexBuffer" => create_buffer(host, call, false),
        "CreateIndexBuffer" => create_buffer(host, call, true),
        "CreateTexture" => create_texture(host, call),
        // `CopyRects(SourceSurface, SourceRects, RectCount, DestSurface,
        // DestPoints)`: RenderWare moves a rendered frame into a texture through
        // here, so the rectangles have to land in the guest's own bytes for the
        // next draw to sample them.
        "CopyRects" => {
            state(host)?;
            let source = parameter(call, 0)?;
            let rects = parameter(call, 1)?;
            let count = parameter(call, 2)?;
            let destination = parameter(call, 3)?;
            let points = parameter(call, 4)?;
            copy_rects(host, call, source, rects, count, destination, points)
        }
        // A volume or cube texture, a texture a guest hands over from an image
        // surface, and the state blocks: each names itself rather than being
        // answered with something that would draw the wrong picture.
        "CreateVolumeTexture" | "CreateCubeTexture" | "UpdateTexture"
        | "CreateAdditionalSwapChain"
        | "CreateStateBlock" | "BeginStateBlock" | "EndStateBlock" | "DeleteStateBlock"
        | "ApplyStateBlock" | "CaptureStateBlock" | "ProcessVertices" | "SetClipPlane"
        | "GetClipPlane" => {
            Err(format!(
                "IDirect3DDevice8.{method} has no shim: the object exists but this method does"
            ))
        }
        // Lighting is a per-vertex computation this host does not do yet. The
        // materials and lights are accepted so the frame still draws -- a game
        // sets them before its first draw -- and the notice says the picture is
        // flat, which is the difference somebody sees rather than reads.
        "SetMaterial" | "SetLight" | "LightEnable" | "GetMaterial" | "GetLight"
        | "GetLightEnable" => {
            if !host.d3d8.said_lighting {
                host.d3d8.said_lighting = true;
                host.notice(
                    "Direct3D 8: lighting is accepted but not applied yet, so geometry is".to_string()
                        + " drawn with its vertex colours and textures only",
                );
            }
            // The setters change nothing this host reads, and the getters answer
            // with the state a light that was never set has: all zero, disabled.
            match method {
                "GetMaterial" => zero(call, parameter(call, 0)?, D3DMATERIAL8_BYTES)?,
                // `GetLight(Index, pLight)`: the buffer is the second argument.
                "GetLight" => zero(call, parameter(call, 1)?, D3DLIGHT8_BYTES)?,
                "GetLightEnable" => {
                    let destination = parameter(call, 1)?;
                    if destination != 0 {
                        call.write_u32(destination, 0)?;
                    }
                }
                _ => {}
            }
            Ok(Return(S_OK))
        }
        // `SetVertexShader` takes either an FVF or a shader handle, and only the
        // value says which. A handle this host handed out is the shader; anything
        // else is the vertex layout the page's fixed-function path reads.
        "SetVertexShader" => {
            let value = parameter(call, 0)?;
            if host.d3d8.shaders.contains(&value) {
                state(host)?;
                if let Some(device) = host.d3d8.device.as_mut() {
                    device.vertex_shader = value;
                }
                shader_notice(host);
                return Ok(Return(S_OK));
            }
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetVertexShader with no device".to_string())?;
            device.fvf = value;
            device.vertex_shader = 0;
            host.d3d8.stream.begin(op::SET_FVF).u32(value);
            Ok(Return(S_OK))
        }
        "CreateVertexShader" | "CreatePixelShader" => {
            state(host)?;
            let destination = parameter(call, 1)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let shader = thunks::new_object(call.memory, host, D3D8_RESOURCE)?;
            host.d3d8.shaders.insert(shader);
            call.write_u32(destination, shader)?;
            shader_notice(host);
            Ok(Return(S_OK))
        }
        "DeleteVertexShader" | "DeletePixelShader" => {
            host.d3d8.shaders.remove(&parameter(call, 0)?);
            if let Some(device) = host.d3d8.device.as_mut() {
                if device.vertex_shader == parameter(call, 0)? {
                    device.vertex_shader = 0;
                }
                if device.pixel_shader == parameter(call, 0)? {
                    device.pixel_shader = 0;
                }
            }
            Ok(Return(S_OK))
        }
        "GetVertexShader" => {
            let destination = parameter(call, 0)?;
            let device = state(host)?;
            let fvf = if device.vertex_shader != 0 {
                device.vertex_shader
            } else {
                device.fvf
            };
            if destination != 0 {
                call.write_u32(destination, fvf)?;
            }
            Ok(Return(S_OK))
        }
        "SetPixelShader" | "SetVertexShaderConstant" | "SetPixelShaderConstant"
        | "SetPaletteEntries" | "SetGammaRamp" | "SetCursorProperties" | "ValidateDevice"
        | "ResourceManagerDiscardBytes" => {
            if method == "SetPixelShader" {
                let value = parameter(call, 0)?;
                if let Some(device) = host.d3d8.device.as_mut() {
                    device.pixel_shader = value;
                }
            }
            if method == "ValidateDevice" {
                let destination = parameter(call, 0)?;
                if destination != 0 {
                    call.write_u32(destination, 1)?;
                }
            }
            Ok(Return(S_OK))
        }
        "GetPixelShader" => {
            let destination = parameter(call, 0)?;
            let shader = state(host)?.pixel_shader;
            if destination != 0 {
                call.write_u32(destination, shader)?;
            }
            Ok(Return(S_OK))
        }
        "GetPaletteEntries" | "GetGammaRamp" => Ok(Return(S_OK)),
        "SetCursorPosition" => Ok(Return(S_OK)),
        "ShowCursor" => Ok(Return(0)),
        "SetTexture" => {
            let stage = parameter(call, 0)?;
            let handle = parameter(call, 1)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetTexture with no device".to_string())?;
            device.textures.insert(stage, handle);
            host.d3d8
                .stream
                .begin(op::SET_TEXTURE)
                .u32(stage)
                .u32(handle);
            Ok(Return(S_OK))
        }
        "GetTexture" => {
            let stage = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            let handle = state(host)?.textures.get(&stage).copied().unwrap_or(0);
            if destination != 0 {
                call.write_u32(destination, handle)?;
            }
            Ok(Return(S_OK))
        }
        "SetStreamSource" => {
            let stream = parameter(call, 0)?;
            let handle = parameter(call, 1)?;
            let stride = parameter(call, 2)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetStreamSource with no device".to_string())?;
            let stride = if stride != 0 {
                stride
            } else {
                fvf_stride(device.fvf).unwrap_or(0)
            };
            device.streams.insert(
                stream,
                Source {
                    buffer: handle,
                    stride,
                    base: 0,
                },
            );
            host.d3d8
                .stream
                .begin(op::SET_STREAM_SOURCE)
                .u32(stream)
                .u32(handle)
                .u32(stride)
                .u32(0);
            Ok(Return(S_OK))
        }
        "GetStreamSource" => {
            let stream = parameter(call, 0)?;
            let handle_at = parameter(call, 1)?;
            let stride_at = parameter(call, 2)?;
            let source = state(host)?.streams.get(&stream).copied();
            if handle_at != 0 {
                call.write_u32(handle_at, source.map_or(0, |source| source.buffer))?;
            }
            if stride_at != 0 {
                call.write_u32(stride_at, source.map_or(0, |source| source.stride))?;
            }
            Ok(Return(S_OK))
        }
        "SetIndices" => {
            let handle = parameter(call, 0)?;
            let base = parameter(call, 1)?;
            let device = host
                .d3d8
                .device
                .as_mut()
                .ok_or_else(|| "Direct3D 8: SetIndices with no device".to_string())?;
            device.indices = if handle == 0 { None } else { Some(handle) };
            device.index_base = base;
            host.d3d8
                .stream
                .begin(op::SET_INDICES)
                .u32(handle)
                .u32(base);
            Ok(Return(S_OK))
        }
        "GetIndices" => {
            let handle_at = parameter(call, 0)?;
            let base_at = parameter(call, 1)?;
            let device = state(host)?;
            let (handle, base) = (device.indices.unwrap_or(0), device.index_base);
            if handle_at != 0 {
                call.write_u32(handle_at, handle)?;
            }
            if base_at != 0 {
                call.write_u32(base_at, base)?;
            }
            Ok(Return(S_OK))
        }
        "DrawPrimitive" => {
            let topology = parameter(call, 0)?;
            let start = parameter(call, 1)?;
            let primitives = parameter(call, 2)?;
            let vertices = Device::vertices(topology, primitives).ok_or_else(|| {
                format!("Direct3D 8: DrawPrimitive asked for primitive type {topology}")
            })?;
            state(host)?;
            host.d3d8.draws += 1;
            host.d3d8
                .stream
                .begin(op::DRAW)
                .u32(topology)
                .u32(start)
                .u32(vertices);
            Ok(Return(S_OK))
        }
        "DrawIndexedPrimitive" => {
            let topology = parameter(call, 0)?;
            let minimum = parameter(call, 1)?;
            let vertices = parameter(call, 2)?;
            let start = parameter(call, 3)?;
            let primitives = parameter(call, 4)?;
            if Device::vertices(topology, primitives).is_none() {
                return Err(format!(
                    "Direct3D 8: DrawIndexedPrimitive asked for primitive type {topology}"
                ));
            }
            state(host)?;
            host.d3d8.draws += 1;
            host.d3d8
                .stream
                .begin(op::DRAW_INDEXED)
                .u32(topology)
                .u32(minimum)
                .u32(vertices)
                .u32(start)
                .u32(primitives);
            Ok(Return(S_OK))
        }
        "DrawPrimitiveUP" => {
            let topology = parameter(call, 0)?;
            let primitives = parameter(call, 1)?;
            let data = parameter(call, 2)?;
            let stride = parameter(call, 3)?;
            let fvf = state(host)?.fvf;
            let stride = if stride != 0 {
                stride
            } else {
                fvf_stride(fvf).ok_or_else(|| {
                    format!("Direct3D 8: DrawPrimitiveUP with no stride and FVF {fvf:#x}")
                })?
            };
            let vertices = Device::vertices(topology, primitives).ok_or_else(|| {
                format!("Direct3D 8: DrawPrimitiveUP asked for primitive type {topology}")
            })?;
            host.d3d8.draws += 1;
            host.d3d8
                .stream
                .begin(op::DRAW_UP)
                .u32(topology)
                .u32(stride)
                .u32(fvf)
                .guest(data)
                .u32(vertices);
            Ok(Return(S_OK))
        }
        "DrawIndexedPrimitiveUP" => {
            let topology = parameter(call, 0)?;
            let minimum = parameter(call, 1)?;
            let vertices = parameter(call, 2)?;
            let primitives = parameter(call, 3)?;
            let indices = parameter(call, 4)?;
            let format = parameter(call, 5)?;
            let data = parameter(call, 6)?;
            let stride = parameter(call, 7)?;
            if format != D3DFMT_INDEX16 {
                return Err(format!(
                    "Direct3D 8: DrawIndexedPrimitiveUP with index format {format}: only 16-bit \
                     indices are supported"
                ));
            }
            let fvf = state(host)?.fvf;
            let stride = if stride != 0 {
                stride
            } else {
                fvf_stride(fvf).ok_or_else(|| {
                    format!("Direct3D 8: DrawIndexedPrimitiveUP with no stride and FVF {fvf:#x}")
                })?
            };
            if Device::vertices(topology, primitives).is_none() {
                return Err(format!(
                    "Direct3D 8: DrawIndexedPrimitiveUP asked for primitive type {topology}"
                ));
            }
            host.d3d8.draws += 1;
            let mut record = host.d3d8.stream.begin(op::DRAW_INDEXED_UP);
            record
                .u32(topology)
                .u32(stride)
                .u32(fvf)
                .guest(indices)
                .guest(data)
                .u32(minimum)
                .u32(vertices)
                .u32(primitives);
            Ok(Return(S_OK))
        }
        _ => Err(format!(
            "IDirect3DDevice8.{method} has no shim: the object exists but this method does"
        )),
    }
}

/// `D3DDEVICE_CAPS`, 212 bytes of members, all dwords except four floats.
///
/// The numbers below come from `d3d8caps.h` in order; the comment on each says
/// what it is, because the offsets are the ABI and a missing member shifts every
/// one after it. What the values claim is what the page's backend can do: one
/// adapter, non-power-of-two textures, eight stages, no vertex or pixel shaders,
/// and no hardware transform. A guest reads these to choose between its own code
/// paths, so a claim it cannot back is a wrong picture rather than a slow one.
fn device_caps(host: &Host, call: &mut Call<'_>, argument: u32) -> Result<Action> {
    let destination = parameter(call, argument)?;
    if destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    // The caps describe the adapter, not a created device: Direct3D 8 lets a guest
    // ask `IDirect3D8` before it has one, and a 2005 game does exactly that to
    // choose the device type and the vertex processing mode it asks for next.
    let _ = host;
    let mut values = [0_u32; 53];
    values[0] = 1; // DeviceType: D3DDEVTYPE_HAL
    values[1] = 0; // AdapterOrdinal
    values[2] = 0x0002_0000; // Caps: D3DCAPS_READ_SCANLINE
    values[3] = 0x0008_0000; // Caps2: D3DCAPS2_CANRENDERWINDOWED
    values[5] = 0x8000_0001; // PresentationIntervals: IMMEDIATE | ONE
    values[6] = 0x1; // CursorCaps: D3DCURSORCAPS_COLOR
    values[7] = 0x10 | 0x40 | 0x100 | 0x200 | 0x400; // DevCaps
    values[8] = 0x2 | 0x4 | 0x8 | 0x20 | 0x40; // PrimitiveMiscCaps: mask z, cull, blend op, colour write
    values[9] = 0x10 | 0x80; // RasterCaps: z test, vertex fog
    values[10] = 0xff; // ZCmpCaps
    values[11] = 0xfff; // SrcBlendCaps
    values[12] = 0xfff; // DestBlendCaps
    values[13] = 0xff; // AlphaCmpCaps
    values[14] = 0x1f_0000; // ShadeCaps: colour/alpha, flat and gouraud
    values[15] = 0; // TextureCaps: non-power-of-two allowed
    values[16] = 0x10 | 0x20 | 0x100 | 0x200; // TextureFilterCaps
    values[19] = 0x1f; // TextureAddressCaps: wrap, mirror, clamp, border, mirror-once
    values[21] = 0x1; // LineCaps: D3DLINECAPS_TEXTURE
    values[22] = 4096; // MaxTextureWidth
    values[23] = 4096; // MaxTextureHeight
    values[25] = 8192; // MaxTextureRepeat
    values[26] = 4096; // MaxTextureAspectRatio
    values[27] = 1; // MaxAnisotropy
    values[28] = 1e10_f32.to_bits(); // MaxVertexW
    values[34] = 0xff; // StencilCaps
    values[35] = 8 | 0x0008_0000; // FVFCaps: eight texcoord sets, do not strip elements
    values[36] = 0xffff; // TextureOpCaps: every operation D3D8 has
    values[37] = 8; // MaxTextureBlendStages
    values[38] = 8; // MaxSimultaneousTextures
    values[39] = 0x1 | 0x4 | 0x8 | 0x10; // VertexProcessingCaps
    values[40] = 8; // MaxActiveLights
    values[44] = 64.0_f32.to_bits(); // MaxPointSize
    values[45] = 0x000f_ffff; // MaxPrimitiveCount
    values[46] = 0xffff; // MaxVertexIndex
    values[47] = 16; // MaxStreams
    values[48] = 255; // MaxStreamStride
    values[49] = 0; // VertexShaderVersion: none. The page draws fixed-function.
    values[50] = 96; // MaxVertexShaderConst
    values[51] = 0; // PixelShaderVersion: none, for the same reason.
    values[52] = 1.0_f32.to_bits(); // MaxPixelShaderValue
    for (index, value) in values.iter().enumerate() {
        call.write_u32(destination + index as u32 * 4, *value)?;
    }
    Ok(Return(S_OK))
}

/// `D3DMATRIX` is sixteen floats in row-major order, which is the order the
/// stream and the page both use. Direct3D itself multiplies row vectors, so this
/// is not transposed anywhere: the page's shader does `mul(vector, matrix)`.
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

fn read_matrix(memory: &crate::memory::GuestMemory, address: u32) -> Result<[f32; 16]> {
    let mut matrix = [0.0_f32; 16];
    for (index, value) in matrix.iter_mut().enumerate() {
        *value = f32::from_bits(memory.read_u32(address + index as u32 * 4)?);
    }
    Ok(matrix)
}

fn write_matrix(call: &mut Call<'_>, address: u32, matrix: &[f32; 16]) -> Result<()> {
    for (index, value) in matrix.iter().enumerate() {
        call.write_u32(address + index as u32 * 4, value.to_bits())?;
    }
    Ok(())
}

/// `left * right`, row-major, which is what `MultiplyTransform` stores.
fn multiply(left: &[f32; 16], right: &[f32; 16]) -> [f32; 16] {
    let mut product = [0.0_f32; 16];
    for row in 0..4 {
        for column in 0..4 {
            let mut sum = 0.0;
            for inner in 0..4 {
                sum += left[row * 4 + inner] * right[inner * 4 + column];
            }
            product[row * 4 + column] = sum;
        }
    }
    product
}

/// `CreateVertexBuffer` and `CreateIndexBuffer`: guest memory the guest writes
/// through `Lock`, and a page-side buffer the draw reads.
fn create_buffer(host: &mut Host, call: &mut Call<'_>, indices: bool) -> Result<Action> {
    let length = parameter(call, 0)?;
    let format = parameter(call, 2)?;
    let destination = parameter(call, 4)?;
    if length == 0 || destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    if indices && format != D3DFMT_INDEX16 {
        return Err(format!(
            "Direct3D 8: CreateIndexBuffer asked for format {format}: only 16-bit indices are \
             supported"
        ));
    }
    let address = host.allocate(length, 4)?;
    let kind = if indices {
        D3D8_INDEX_BUFFER
    } else {
        D3D8_VERTEX_BUFFER
    };
    let object = thunks::new_object(call.memory, host, kind)?;
    let buffer = Buffer {
        size: length,
        address,
        locks: Vec::new(),
        format,
    };
    if indices {
        host.d3d8.index_buffers.insert(object, buffer);
        host.d3d8
            .stream
            .begin(op::CREATE_INDEX_BUFFER)
            .u32(object)
            .u32(length);
    } else {
        host.d3d8.vertex_buffers.insert(object, buffer);
        host.d3d8
            .stream
            .begin(op::CREATE_VERTEX_BUFFER)
            .u32(object)
            .u32(length)
            .u32(format);
    }
    call.write_u32(destination, object)?;
    Ok(Return(S_OK))
}

/// `IDirect3DVertexBuffer8` and `IDirect3DIndexBuffer8`: the lock that hands the
/// guest the bytes, the descriptor that describes them, and nothing else.
fn buffer(method: &str, host: &mut Host, call: &mut Call<'_>, indices: bool) -> Result<Action> {
    match method {
        "Lock" => {
            let offset = parameter(call, 0)?;
            let size = parameter(call, 1)?;
            let destination = parameter(call, 2)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let object = call.arg(0)?;
            let table = if indices {
                &mut host.d3d8.index_buffers
            } else {
                &mut host.d3d8.vertex_buffers
            };
            let Some(buffer) = table.get_mut(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if offset > buffer.size {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let length = if size == 0 { buffer.size - offset } else { size };
            if length > buffer.size - offset {
                return Err(format!(
                    "Direct3D 8: a lock of {length} bytes at {offset} runs past a {} byte buffer",
                    buffer.size
                ));
            }
            buffer.locks.push((offset, length));
            let pointer = buffer.address + offset;
            call.write_u32(destination, pointer)?;
            Ok(Return(S_OK))
        }
        "Unlock" => {
            let object = call.arg(0)?;
            let table = if indices {
                &mut host.d3d8.index_buffers
            } else {
                &mut host.d3d8.vertex_buffers
            };
            let Some(buffer) = table.get_mut(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let Some((offset, length)) = buffer.locks.pop() else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let address = buffer.address + offset;
            let op = if indices {
                op::UPDATE_INDEX_BUFFER
            } else {
                op::UPDATE_VERTEX_BUFFER
            };
            let mut record = host.d3d8.stream.begin(op);
            record
                .u32(object)
                .u32(offset)
                .u32(length)
                .guest(address);
            Ok(Return(S_OK))
        }
        "GetDesc" => {
            let destination = parameter(call, 0)?;
            let object = call.arg(0)?;
            let table = if indices {
                &host.d3d8.index_buffers
            } else {
                &host.d3d8.vertex_buffers
            };
            let Some(buffer) = table.get(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if destination != 0 {
                call.write_u32(destination, buffer.format)?; // Format
                // Type: D3DRTYPE_INDEXBUFFER is 7, D3DRTYPE_VERTEXBUFFER is 6.
                call.write_u32(destination + 4, if indices { 7 } else { 6 })?;
                call.write_u32(destination + 8, 0)?; // Usage
                call.write_u32(destination + 12, 1)?; // Pool: D3DPOOL_MANAGED
                call.write_u32(destination + 16, buffer.size)?;
                if !indices {
                    call.write_u32(destination + 20, buffer.format)?; // FVF
                }
            }
            Ok(Return(S_OK))
        }
        _ => Err(format!(
            "{}.{method} has no shim: the object exists but this method does",
            if indices {
                "IDirect3DIndexBuffer8"
            } else {
                "IDirect3DVertexBuffer8"
            }
        )),
    }
}

/// `CreateTexture`: level 0 only, in a format the page's texture upload can read.
///
/// A level count of zero means the full chain, which is remembered so
/// `GetLevelCount` answers what the guest asked for; the extra levels are never
/// filled because nothing samples them.
static TEXTURE_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn create_texture(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let width = parameter(call, 0)?;
    let height = parameter(call, 1)?;
    let levels = parameter(call, 2)?;
    let format = parameter(call, 4)?;
    let destination = parameter(call, 6)?;
    if width == 0 || height == 0 || destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    let Some(bytes) = bytes_per_pixel(format) else {
        return Err(format!(
            "Direct3D 8: CreateTexture asked for format {format}, which this host does not \
             convert yet"
        ));
    };
    // A level count of zero is the full chain, which Direct3D sizes down to 1x1.
    let levels = if levels == 0 {
        let mut count = 1;
        let (mut w, mut h) = (width, height);
        while w > 1 || h > 1 {
            w = (w / 2).max(1);
            h = (h / 2).max(1);
            count += 1;
        }
        count
    } else {
        levels
    };
    let pitch = (width * bytes + 3) & !3;
    let mut level_sizes = vec![pitch * height];
    let mut level_pitches = vec![pitch];
    let (mut w, mut h) = (width, height);
    for _ in 1..levels {
        w = (w / 2).max(1);
        h = (h / 2).max(1);
        let level_pitch = (w * bytes + 3) & !3;
        level_pitches.push(level_pitch);
        level_sizes.push(level_pitch * h);
    }
    if std::env::var("SHIMKIT_LOG_TEXTURES").is_ok() {
        let count = TEXTURE_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        eprintln!("texture {count}: {width}x{height} levels {levels} format {format:#x}");
    }
    let total: u32 = level_sizes.iter().sum();
    let address = host.allocate(total, 4)?;
    let object = thunks::new_object(call.memory, host, D3D8_TEXTURE)?;
    host.d3d8
        .stream
        .begin(op::CREATE_TEXTURE)
        .u32(object)
        .u32(width)
        .u32(height)
        .u32(levels)
        .u32(format)
        .u32(level_sizes[0]);
    host.d3d8.textures.insert(
        object,
        Texture {
            width,
            height,
            levels,
            format,
            address,
            size: level_sizes[0],
            pitch,
            level_sizes,
            level_pitches,
        },
    );
    call.write_u32(destination, object)?;
    Ok(Return(S_OK))
}

/// `IDirect3DTexture8`: `LockRect` hands out level 0's pixels, `UnlockRect`
/// uploads them, and the level queries describe them.
fn texture(method: &str, host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    match method {
        "LockRect" => {
            let level = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let object = call.arg(0)?;
            let Some(texture) = host.d3d8.textures.get(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let Some(address) = texture.level_address(level) else {
                return Err(format!(
                    "Direct3D 8: IDirect3DTexture8.LockRect asked for level {level} of a {}-level \
                     texture",
                    texture.levels
                ));
            };
            call.write_u32(destination, texture.level_pitches[level as usize])?;
            call.write_u32(destination + 4, address)?;
            Ok(Return(S_OK))
        }
        "UnlockRect" => {
            let level = parameter(call, 0)?;
            let object = call.arg(0)?;
            let Some(texture) = host.d3d8.textures.get(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let Some(address) = texture.level_address(level) else {
                return Err(format!(
                    "Direct3D 8: IDirect3DTexture8.UnlockRect asked for level {level} of a \
                     {}-level texture",
                    texture.levels
                ));
            };
            let (width, height, format, pitch) = (
                texture.level_width(level),
                texture.level_height(level),
                texture.format,
                texture.level_pitches[level as usize],
            );
            let mut record = host.d3d8.stream.begin(op::UPDATE_TEXTURE);
            record
                .u32(object)
                .u32(level)
                .guest(address)
                .u32(pitch)
                .u32(width)
                .u32(height)
                .u32(format);
            Ok(Return(S_OK))
        }
        "GetLevelCount" => {
            let object = call.arg(0)?;
            let levels = host.d3d8.textures.get(&object).map_or(1, |t| t.levels);
            Ok(Return(levels))
        }
        "GetLevelDesc" => {
            let level = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            let object = call.arg(0)?;
            let Some(texture) = host.d3d8.textures.get(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if level >= texture.levels {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            if destination != 0 {
                call.write_u32(destination, texture.format)?;
                call.write_u32(destination + 4, 3)?; // D3DRTYPE_TEXTURE
                call.write_u32(destination + 8, 0)?; // Usage
                call.write_u32(destination + 12, 1)?; // Pool: D3DPOOL_MANAGED
                call.write_u32(destination + 16, texture.level_sizes[level as usize])?;
                call.write_u32(destination + 20, 0)?; // MultiSampleType
                call.write_u32(destination + 24, texture.level_width(level))?;
                call.write_u32(destination + 28, texture.level_height(level))?;
            }
            Ok(Return(S_OK))
        }
        "GetSurfaceLevel" => {
            let level = parameter(call, 0)?;
            let destination = parameter(call, 1)?;
            let object = call.arg(0)?;
            let Some(t) = host.d3d8.textures.get(&object) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if level >= t.levels {
                return Err(format!(
                    "Direct3D 8: IDirect3DTexture8.GetSurfaceLevel asked for level {level} of a \
                     {}-level texture",
                    t.levels
                ));
            }
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            // The surface is a window onto the texture's level: the bytes are
            // the texture's own, which the page already reads, so the surface
            // only has to remember which texture and level it belongs to.
            let surface = thunks::new_object(call.memory, host, D3D8_SURFACE)?;
            host.d3d8.texture_surfaces.insert(surface, (object, level));
            call.write_u32(destination, surface)?;
            Ok(Return(S_OK))
        }
        _ => Err(format!(
            "IDirect3DTexture8.{method} has no shim: the object exists but this method does"
        )),
    }
}

/// `IDirect3DSwapChain8`: the device's own swap chain is the canvas, and
/// `CreateAdditionalSwapChain` is refused, so no swap chain object is handed out.
/// The interface is still routed here rather than left as an unknown kind, so a
/// guest that somehow reaches one gets a named method instead of a slot number.
fn swap_chain(method: &str, _host: &mut Host, _call: &mut Call<'_>) -> Result<Action> {
    Err(format!(
        "IDirect3DSwapChain8.{method} has no shim: the object exists but this method does"
    ))
}

/// The `IDirect3DSurface8` methods.
///
/// A surface here is a name for a buffer rather than pixels of its own: the
/// page draws into one buffer, and the created surfaces are records a guest
/// reads back. `GetDesc` answers what the device knows about the surface,
/// which is what a guest sizes its copies and picks its formats from, so a
/// wrong answer there is a wrong allocation later.
fn surface(method: &str, host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let object = call.arg(0)?;
    if let Some(&(texture, level)) = host.d3d8.texture_surfaces.get(&object) {
        return texture_surface(method, host, call, texture, level);
    }
    match method {
        "GetDesc" => {
            let destination = parameter(call, 0)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let (width, height, format, pool, usage) = if object == host.d3d8.back_buffer_surface {
                let device = state(host)?;
                (
                    device.width,
                    device.height,
                    D3DFMT_X8R8G8B8,
                    D3DPOOL_DEFAULT,
                    D3DUSAGE_RENDERTARGET,
                )
            } else if object == host.d3d8.depth_surface {
                let device = state(host)?;
                if device.depth_format == 0 {
                    return Err(
                        "Direct3D 8: GetDesc ran on a depth surface, and the device was not \
                         created with an auto depth-stencil to describe"
                            .to_string(),
                    );
                }
                (
                    device.width,
                    device.height,
                    device.depth_format,
                    D3DPOOL_DEFAULT,
                    D3DUSAGE_DEPTHSTENCIL,
                )
            } else if let Some(surface) = host.d3d8.surfaces.get(&object) {
                (surface.width, surface.height, surface.format, surface.pool, surface.usage)
            } else {
                return Err(format!(
                    "Direct3D 8: GetDesc ran on a surface ({object:#x}) this host never made"
                ));
            };
            // `D3DSURFACE_DESC`: format, type, usage, pool, size, multi-sample,
            // width, height. The size is the bytes the surface occupies, which
            // is what a guest sizes a lock or a copy from; a format whose size
            // this host does not track answers zero rather than a guess.
            call.write_u32(destination, format)?;
            call.write_u32(destination + 4, D3DRTYPE_SURFACE)?;
            call.write_u32(destination + 8, usage)?;
            call.write_u32(destination + 12, pool)?;
            call.write_u32(destination + 16, surface_bytes(format, width, height))?;
            call.write_u32(destination + 20, 0)?; // D3DMULTISAMPLE_NONE
            call.write_u32(destination + 24, width)?;
            call.write_u32(destination + 28, height)?;
            Ok(Return(S_OK))
        }
        _ => Err(format!(
            "IDirect3DSurface8.{method} has no shim: the object exists but this method does"
        )),
    }
}

/// The `IDirect3DSurface8` methods on a surface that is a texture's level.
///
/// The lock is the texture's own `LockRect` — the same guest bytes and the same
/// pitch — and the unlock records the texture level for the page, exactly as
/// locking the texture directly would. A guest that fills a texture through its
/// surface therefore behaves like one that locks the texture, and the two roads
/// meet in the same command-stream record.
fn texture_surface(
    method: &str,
    host: &mut Host,
    call: &mut Call<'_>,
    texture: u32,
    level: u32,
) -> Result<Action> {
    match method {
        "GetDesc" => {
            let destination = parameter(call, 0)?;
            let Some(t) = host.d3d8.textures.get(&texture) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if destination != 0 {
                call.write_u32(destination, t.format)?;
                call.write_u32(destination + 4, D3DRTYPE_SURFACE)?;
                call.write_u32(destination + 8, 0)?; // Usage
                call.write_u32(destination + 12, 1)?; // Pool: D3DPOOL_MANAGED
                call.write_u32(destination + 16, t.level_sizes[level as usize])?;
                call.write_u32(destination + 20, 0)?; // D3DMULTISAMPLE_NONE
                call.write_u32(destination + 24, t.level_width(level))?;
                call.write_u32(destination + 28, t.level_height(level))?;
            }
            Ok(Return(S_OK))
        }
        "LockRect" => {
            let destination = parameter(call, 0)?;
            if destination == 0 {
                return Ok(Return(D3DERR_INVALIDCALL));
            }
            let Some(t) = host.d3d8.textures.get(&texture) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            if std::env::var("SHIMKIT_LOG_TEXTURES").is_ok() {
                let rect = parameter(call, 1).unwrap_or(0);
                let (left, top, right, bottom) = if rect != 0 {
                    (
                        call.memory.read_u32(rect).unwrap_or(0),
                        call.memory.read_u32(rect + 4).unwrap_or(0),
                        call.memory.read_u32(rect + 8).unwrap_or(0),
                        call.memory.read_u32(rect + 12).unwrap_or(0),
                    )
                } else {
                    (0, 0, 0, 0)
                };
                eprintln!(
                    "lock level {level} of {}x{}: rect {left},{top}..{right},{bottom}",
                    t.level_width(level),
                    t.level_height(level)
                );
            }
            let Some(address) = t.level_address(level) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            call.write_u32(destination, t.level_pitches[level as usize])?;
            call.write_u32(destination + 4, address)?;
            Ok(Return(S_OK))
        }
        "UnlockRect" => {
            let Some(t) = host.d3d8.textures.get(&texture) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let Some(address) = t.level_address(level) else {
                return Ok(Return(D3DERR_INVALIDCALL));
            };
            let (width, height, format, pitch) = (
                t.level_width(level),
                t.level_height(level),
                t.format,
                t.level_pitches[level as usize],
            );
            let mut record = host.d3d8.stream.begin(op::UPDATE_TEXTURE);
            record
                .u32(texture)
                .u32(level)
                .guest(address)
                .u32(pitch)
                .u32(width)
                .u32(height)
                .u32(format);
            Ok(Return(S_OK))
        }
        _ => Err(format!(
            "IDirect3DSurface8.{method} has no shim: the object exists but this method does"
        )),
    }
}

/// The guest bytes behind a surface, when this device has any.
///
/// Only a texture's levels are real memory here: the back buffer and the
/// surfaces `CreateRenderTarget` and `CreateImageSurface` hand out are names
/// for buffers the page owns, so a copy into or out of one of them has no
/// bytes here to move.
#[derive(Clone, Copy)]
struct Pixels {
    address: u32,
    pitch: u32,
    width: u32,
    height: u32,
    format: u32,
    bytes: u32,
}

fn pixels(host: &Host, object: u32) -> Option<Pixels> {
    let &(texture, level) = host.d3d8.texture_surfaces.get(&object)?;
    let t = host.d3d8.textures.get(&texture)?;
    Some(Pixels {
        address: t.level_address(level)?,
        pitch: t.level_pitches[level as usize],
        width: t.level_width(level),
        height: t.level_height(level),
        format: t.format,
        bytes: bytes_per_pixel(t.format)?,
    })
}

/// `CopyRects`: each source rectangle lands at the matching destination point,
/// or at its own coordinates when no points are given. A zero rectangle count
/// with no rectangle array means the whole surface.
///
/// Direct3D 8 requires the two surfaces to share a format, and a rectangle that
/// reaches past either surface is clipped rather than wrapped: a wrapped row
/// would draw the picture skewed. A copy into a texture level is followed by the
/// same upload an unlock records, because the next draw samples those bytes.
fn copy_rects(
    host: &mut Host,
    call: &mut Call<'_>,
    source: u32,
    rects: u32,
    count: u32,
    destination: u32,
    points: u32,
) -> Result<Action> {
    if source == 0 || destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    let (Some(from), Some(to)) = (pixels(host, source), pixels(host, destination)) else {
        // A rendered frame copied out of the back buffer is the common case, and
        // the page already has those pixels on its own side of the fence.
        if !host.d3d8.said_copy_rects {
            host.d3d8.said_copy_rects = true;
            host.notice(format!(
                "Direct3D 8: CopyRects moves pixels between surfaces the page owns \
                 ({source:#x} -> {destination:#x}, {count} rectangle(s)), which this host has \
                 no guest bytes for; the texture keeps what it held"
            ));
        }
        return Ok(Return(S_OK));
    };
    if from.format != to.format {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    if rects == 0 || count == 0 {
        copy_rectangle(call, &from, &to, 0, 0, from.width as i32, from.height as i32, 0, 0)?;
    } else {
        for index in 0..count {
            let rect = rects.wrapping_add(index.wrapping_mul(16));
            let left = call.memory.read_u32(rect)? as i32;
            let top = call.memory.read_u32(rect + 4)? as i32;
            let right = call.memory.read_u32(rect + 8)? as i32;
            let bottom = call.memory.read_u32(rect + 12)? as i32;
            let (dx, dy) = if points == 0 {
                (left, top)
            } else {
                let point = points.wrapping_add(index.wrapping_mul(8));
                (
                    call.memory.read_u32(point)? as i32,
                    call.memory.read_u32(point + 4)? as i32,
                )
            };
            copy_rectangle(call, &from, &to, left, top, right, bottom, dx, dy)?;
        }
    }
    if let Some(&(texture, level)) = host.d3d8.texture_surfaces.get(&destination) {
        let mut record = host.d3d8.stream.begin(op::UPDATE_TEXTURE);
        record
            .u32(texture)
            .u32(level)
            .guest(to.address)
            .u32(to.pitch)
            .u32(to.width)
            .u32(to.height)
            .u32(to.format);
    }
    Ok(Return(S_OK))
}

/// Copies the part of `(left, top)-(right, bottom)` that lies inside both
/// surfaces, with its top-left corner at `(dx, dy)` on the destination.
#[allow(clippy::too_many_arguments)]
fn copy_rectangle(
    call: &mut Call<'_>,
    from: &Pixels,
    to: &Pixels,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    dx: i32,
    dy: i32,
) -> Result<()> {
    let (width, height) = (from.width as i32, from.height as i32);
    let mut left = left.max(0);
    let mut top = top.max(0);
    let mut right = right.min(width);
    let mut bottom = bottom.min(height);
    // The destination point can push the copied block off the destination, in
    // either direction, so the rectangle moves rather than the point.
    left = left.max(-dx);
    top = top.max(-dy);
    right = right.min(to.width as i32 - dx);
    bottom = bottom.min(to.height as i32 - dy);
    if right <= left || bottom <= top {
        return Ok(());
    }
    let row = (right - left) as usize * from.bytes as usize;
    let (x, y) = ((dx + left) as u32, (dy + top) as u32);
    for row_index in 0..(bottom - top) as u32 {
        let source = from.address
            + (top as u32 + row_index) * from.pitch
            + left as u32 * from.bytes;
        let destination = to.address + (y + row_index) * to.pitch + x * to.bytes;
        call.memory.copy(source, destination, row)?;
    }
    Ok(())
}

/// `QueryInterface`: `IID_IUnknown` answers the same object, and anything else is
/// refused. Answering `S_OK` for an interface that is not implemented would send
/// the guest into a vtable we cannot answer, which is worse than a stop.
fn query_interface(call: &mut Call<'_>) -> Result<Action> {
    let riid = parameter(call, 0)?;
    let destination = parameter(call, 1)?;
    if destination == 0 {
        return Ok(Return(D3DERR_INVALIDCALL));
    }
    let mut unknown = true;
    for (index, expected) in IID_UNKNOWN.iter().enumerate() {
        if call.memory.read_u8(riid.wrapping_add(index as u32))? != *expected {
            unknown = false;
            break;
        }
    }
    if unknown {
        call.write_u32(destination, call.arg(0)?)?;
        Ok(Return(S_OK))
    } else {
        call.write_u32(destination, 0)?;
        Ok(Return(E_NOINTERFACE))
    }
}

/// `Direct3DCreate8(sdk)`: the one export. It takes the SDK version and returns
/// the interface in `eax` -- there is no out-parameter, unlike
/// `DirectDrawCreate` -- so the object is the return value. A version this build
/// does not speak answers `NULL`, which is what the real one does, and the guest
/// then reports that it could not start Direct3D rather than calling into a
/// device that is not there.
fn direct3d_create8(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let version = call.arg(0)?;
    if version != D3D_SDK_VERSION {
        host.notice(format!(
            "Direct3D 8: Direct3DCreate8 asked for SDK version {version}, this is {D3D_SDK_VERSION}"
        ));
        return Ok(Return(0));
    }
    let object = thunks::new_object(call.memory, host, D3D8)?;
    host.d3d8.parent = object;
    Ok(Return(object))
}

/// The one export `d3d8.dll` has.
pub static SHIMS: &[Binding] = &[Binding {
    dll: D3D8_DLL,
    name: "Direct3DCreate8",
    arg_bytes: 4,
    shim: direct3d_create8,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::GuestMemory;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    #[test]
    fn every_interface_table_fits_its_vtable() {
        for &(kind, name, table, args) in INTERFACES {
            assert!(handles(kind), "{name} is not handled");
            assert!(
                table.len() <= usize::from(thunks::SLOT_LIMIT),
                "{name} is longer than the thunk space"
            );
            assert_eq!(
                args.len(),
                table.len(),
                "{name} has an argument count for every slot"
            );
            for slot in 0..table.len() {
                let bytes = arg_bytes(kind, slot as u8);
                assert!(
                    bytes >= 4 && bytes % 4 == 0,
                    "{name}.{} pops {bytes} bytes, which is not a whole number of arguments",
                    table[slot]
                );
            }
            // `this` is always pushed, and QueryInterface adds two more.
            assert_eq!(arg_bytes(kind, 0), 12, "{name}.QueryInterface");
            assert_eq!(arg_bytes(kind, 1), 4, "{name}.AddRef");
            assert_eq!(arg_bytes(kind, 2), 4, "{name}.Release");
            assert_eq!(table[0], "QueryInterface", "{name}");
            assert_eq!(table[1], "AddRef", "{name}");
            assert_eq!(table[2], "Release", "{name}");
            // One past the last slot is out of the vtable, not a name.
            assert!(label(kind, table.len() as u8).contains("slot"), "{name}");
        }
    }

    #[test]
    fn the_device_vtable_has_ninety_seven_slots() {
        let device = names(D3D8_DEVICE).expect("the device is modelled");
        assert_eq!(device.len(), 97);
        assert_eq!(label(D3D8_DEVICE, 50), "IDirect3DDevice8.SetRenderState");
        assert_eq!(label(D3D8_DEVICE, 96), "IDirect3DDevice8.DeletePatch");
        // The three resource slots the device shares with every other interface.
        assert_eq!(label(D3D8_DEVICE, 2), "IDirect3DDevice8.Release");
    }

    #[test]
    fn a_surface_getdesc_answers_only_surfaces_this_host_made() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A surface cannot be read back: the page's back buffer is the guest's
        // render target, so `GetDesc` on a surface no method of this host handed
        // out is a stop rather than a zeroed struct, which a guest would size
        // its copies from.
        let destination = crate::memory::STACK_BASE + 0x6000;
        match invoke_on(&mut host, &mut memory, 0x00d0_0001, D3D8_SURFACE, "GetDesc", &[
            destination,
        ]) {
            Err(error) => assert!(error.contains("GetDesc ran on a surface"), "{error}"),
            Ok(_) => panic!("a GetDesc on a surface this host never made answered"),
        }
        // The methods that need contents a surface here does not have yet name
        // themselves.
        match invoke_on(&mut host, &mut memory, 0x00d0_0001, D3D8_SURFACE, "LockRect", &[
            destination,
            destination,
            0,
            0,
        ]) {
            Err(error) => assert!(error.contains("IDirect3DSurface8.LockRect has no shim"), "{error}"),
            Ok(_) => panic!("a surface LockRect answered"),
        }
    }

    #[test]
    fn the_back_buffer_describes_the_page_drawing() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let parameters = crate::memory::STACK_BASE + 0x5100;
        let returned = crate::memory::STACK_BASE + 0x5200;
        memory.write_u32(parameters, 640).unwrap();
        memory.write_u32(parameters + 4, 480).unwrap();
        memory
            .write_u32(parameters + 8, D3DFMT_X8R8G8B8)
            .unwrap();
        invoke(
            &mut host,
            &mut memory,
            D3D8,
            "CreateDevice",
            &[0, 1, 0, 0x20, parameters, returned],
        )
        .expect("a device is created");

        let holder = crate::memory::STACK_BASE + 0x5300;
        invoke(&mut host, &mut memory, D3D8_DEVICE, "GetRenderTarget", &[holder])
            .expect("a render target");
        let surface = memory.read_u32(holder).unwrap();

        let desc = crate::memory::STACK_BASE + 0x5400;
        invoke_on(&mut host, &mut memory, surface, D3D8_SURFACE, "GetDesc", &[desc])
            .expect("a surface description");
        // `D3DSURFACE_DESC` in order: the page draws X8R8G8B8, the kind is
        // D3DRTYPE_SURFACE, a render target, in the default pool, the bytes a
        // 640x480 four-byte surface takes, no multi-sample, and the size.
        assert_eq!(memory.read_u32(desc).unwrap(), D3DFMT_X8R8G8B8, "Format");
        assert_eq!(memory.read_u32(desc + 4).unwrap(), 1, "Type is D3DRTYPE_SURFACE");
        assert_eq!(memory.read_u32(desc + 8).unwrap(), 1, "Usage is D3DUSAGE_RENDERTARGET");
        assert_eq!(memory.read_u32(desc + 12).unwrap(), 0, "Pool is D3DPOOL_DEFAULT");
        assert_eq!(memory.read_u32(desc + 16).unwrap(), 640 * 480 * 4, "Size");
        assert_eq!(memory.read_u32(desc + 20).unwrap(), 0, "MultiSampleType is none");
        assert_eq!(memory.read_u32(desc + 24).unwrap(), 640, "Width");
        assert_eq!(memory.read_u32(desc + 28).unwrap(), 480, "Height");
    }

    #[test]
    fn the_depth_surface_describes_what_the_guest_asked_for() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A device created with an auto depth-stencil in D3DFMT_D16.
        let parameters = crate::memory::STACK_BASE + 0x5100;
        let returned = crate::memory::STACK_BASE + 0x5200;
        memory.write_u32(parameters, 640).unwrap();
        memory.write_u32(parameters + 4, 480).unwrap();
        memory
            .write_u32(parameters + 8, D3DFMT_X8R8G8B8)
            .unwrap();
        memory.write_u32(parameters + 32, 1).unwrap(); // EnableAutoDepthStencil
        memory.write_u32(parameters + 36, D3DFMT_D16).unwrap();
        invoke(
            &mut host,
            &mut memory,
            D3D8,
            "CreateDevice",
            &[0, 1, 0, 0x20, parameters, returned],
        )
        .expect("a device is created");

        let holder = crate::memory::STACK_BASE + 0x5300;
        invoke(
            &mut host,
            &mut memory,
            D3D8_DEVICE,
            "GetDepthStencilSurface",
            &[holder],
        )
        .expect("a depth stencil");
        let surface = memory.read_u32(holder).unwrap();

        let desc = crate::memory::STACK_BASE + 0x5400;
        invoke_on(&mut host, &mut memory, surface, D3D8_SURFACE, "GetDesc", &[desc])
            .expect("a surface description");
        assert_eq!(memory.read_u32(desc).unwrap(), D3DFMT_D16, "Format is what was asked");
        assert_eq!(memory.read_u32(desc + 8).unwrap(), 2, "Usage is D3DUSAGE_DEPTHSTENCIL");
        assert_eq!(memory.read_u32(desc + 16).unwrap(), 640 * 480 * 2, "Size");
        assert_eq!(memory.read_u32(desc + 24).unwrap(), 640, "Width");
        assert_eq!(memory.read_u32(desc + 28).unwrap(), 480, "Height");
    }

    #[test]
    fn created_surfaces_describe_what_was_created() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = device_at(&mut host, &mut memory);

        // `CreateRenderTarget(Width, Height, Format, MultiSample, Lockable,
        // ppRenderTarget)`: the surface lands in the pointer, not in the
        // lockable flag before it.
        let holder = crate::memory::STACK_BASE + 0x5300;
        let desc = crate::memory::STACK_BASE + 0x5400;
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "CreateRenderTarget",
            &[64, 32, D3DFMT_X8R8G8B8, 0, 1, holder],
        )
        .expect("a render target");
        let target = memory.read_u32(holder).unwrap();
        assert!(target != 0 && target != 1, "the surface object went to the pointer");
        invoke_on(&mut host, &mut memory, target, D3D8_SURFACE, "GetDesc", &[desc])
            .expect("a surface description");
        assert_eq!(memory.read_u32(desc + 8).unwrap(), 1, "Usage is D3DUSAGE_RENDERTARGET");
        assert_eq!(memory.read_u32(desc + 12).unwrap(), 0, "Pool is D3DPOOL_DEFAULT");
        assert_eq!(memory.read_u32(desc + 24).unwrap(), 64, "Width");
        assert_eq!(memory.read_u32(desc + 28).unwrap(), 32, "Height");

        // `CreateImageSurface(Width, Height, Format, ppSurface)`: no pool
        // parameter, because an image surface is always system memory.
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "CreateImageSurface",
            &[16, 16, D3DFMT_A8R8G8B8, holder],
        )
        .expect("an image surface");
        let image = memory.read_u32(holder).unwrap();
        invoke_on(&mut host, &mut memory, image, D3D8_SURFACE, "GetDesc", &[desc])
            .expect("a surface description");
        assert_eq!(memory.read_u32(desc + 8).unwrap(), 0, "Usage is none");
        assert_eq!(memory.read_u32(desc + 12).unwrap(), 2, "Pool is D3DPOOL_SYSTEMMEM");
        assert_eq!(memory.read_u32(desc + 16).unwrap(), 16 * 16 * 4, "Size");
    }

    #[test]
    fn create8_returns_the_interface_in_eax() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == "Direct3DCreate8")
            .expect("the export is declared");
        // One argument, so the callee pops exactly one dword. An out-parameter
        // would leave four bytes of the caller's stack behind on return.
        assert_eq!(binding.arg_bytes, 4);

        let args = crate::memory::STACK_BASE + 0x5000;
        memory.write_u32(args, D3D_SDK_VERSION).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        let answer = (binding.shim)(&mut host, &mut call).expect("create answers");
        let Return(object) = answer else {
            panic!("create returned {answer:?}")
        };
        assert!(object > 0, "a supported SDK version gets an object");
        // The object holds its vtable pointer, and slot 3 is
        // IDirect3D8::RegisterSoftwareDevice.
        let table = call.memory.read_u32(object).unwrap();
        assert_eq!(thunks::at(call.memory.read_u32(table + 12).unwrap()), Some((D3D8, 3)));

        // An older SDK version is refused the way the real export refuses it.
        call.memory.write_u32(args, D3D_SDK_VERSION - 1).unwrap();
        assert_eq!(
            (binding.shim)(&mut host, &mut call).unwrap(),
            Return(0),
            "an unsupported SDK version gets NULL"
        );
    }

    /// The slot a method occupies, by name, so a test names the method rather
    /// than counting table entries.
    fn slot(kind: u8, name: &str) -> u8 {
        let table = names(kind).expect("the interface is modelled");
        table
            .iter()
            .position(|entry| *entry == name)
            .unwrap_or_else(|| panic!("{name} is in the vtable")) as u8
    }

    /// Runs one method with `arguments` on the guest's stack, which is where a
    /// thunk call finds them.
    fn invoke(
        host: &mut Host,
        memory: &mut GuestMemory,
        kind: u8,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        invoke_on(host, memory, 0x00d0_0000, kind, name, arguments)
    }

    /// Runs one method on a named object. `this` is the first stack argument of a
    /// COM call, and the parameters follow it exactly as the guest pushes them.
    fn invoke_on(
        host: &mut Host,
        memory: &mut GuestMemory,
        object: u32,
        kind: u8,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let at = crate::memory::STACK_BASE + 0x4000;
        memory.write_u32(at, object).expect("this fits the stack");
        for (index, value) in arguments.iter().enumerate() {
            memory
                .write_u32(at + (index as u32 + 1) * 4, *value)
                .expect("arguments fit the stack");
        }
        let slot = slot(kind, name);
        let mut call = Call {
            memory,
            args: at,
            arg_bytes: arg_bytes(kind, slot),
        };
        serve(host, &mut call, kind, slot)
    }

    #[test]
    fn a_draw_follows_the_state_set_before_it() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // Direct3DCreate8 first, so `GetDirect3D` has an answer and the device has
        // a parent.
        let args = crate::memory::STACK_BASE + 0x5000;
        memory.write_u32(args, D3D_SDK_VERSION).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 4,
        };
        assert!(matches!(
            (SHIMS[0].shim)(&mut host, &mut call).unwrap(),
            Return(object) if object != 0
        ));

        // A 640x480 X8R8G8B8 back buffer.
        let parameters = crate::memory::STACK_BASE + 0x5100;
        let returned = crate::memory::STACK_BASE + 0x5200;
        call.memory.write_u32(parameters, 640).unwrap();
        call.memory.write_u32(parameters + 4, 480).unwrap();
        call.memory.write_u32(parameters + 8, D3DFMT_X8R8G8B8).unwrap();
        let answer = invoke(
            &mut host,
            call.memory,
            D3D8,
            "CreateDevice",
            &[0, 1, 0, 0x20, parameters, returned],
        )
        .expect("a device is created");
        assert_eq!(answer, Return(S_OK));
        let device = call.memory.read_u32(returned).unwrap();
        assert_eq!(host.d3d8.device_object, device);
        assert_eq!(host.d3d8.device.as_ref().unwrap().width, 640);

        // SetRenderState(D3DRS_ZENABLE, TRUE), as every 3D frame starts.
        invoke(&mut host, call.memory, D3D8_DEVICE, "SetRenderState", &[7, 1])
            .expect("render state");

        // A world matrix, then a two-triangle list.
        let matrix = crate::memory::STACK_BASE + 0x5300;
        for index in 0..16_u32 {
            let value = if index % 5 == 0 { 2.0_f32 } else { 0.0 };
            call.memory
                .write_u32(matrix + index * 4, value.to_bits())
                .unwrap();
        }
        invoke(
            &mut host,
            call.memory,
            D3D8_DEVICE,
            "SetTransform",
            &[1, matrix],
        )
        .expect("a transform");
        invoke(
            &mut host,
            call.memory,
            D3D8_DEVICE,
            "DrawPrimitive",
            &[D3DPT_TRIANGLELIST, 0, 2],
        )
        .expect("a draw");

        // The records are in call order, which is the contract: a draw replayed
        // before its matrix draws in the wrong place.
        let mut cursor = crate::render::Cursor::new(host.d3d8.stream.bytes());
        let (first, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(first, op::CREATE_DEVICE);
        assert_eq!(crate::render::payload_u32(payload, 0), Some(640));
        assert_eq!(crate::render::payload_u32(payload, 1), Some(480));
        assert_eq!(crate::render::payload_u32(payload, 2), Some(32));
        let (second, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(second, op::SET_RENDER_STATE);
        assert_eq!(crate::render::payload_u32(payload, 0), Some(7));
        let (third, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(third, op::SET_TRANSFORM);
        assert_eq!(crate::render::payload_u32(payload, 0), Some(1));
        assert_eq!(crate::render::payload_f32(payload, 1), Some(2.0));
        let (fourth, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(fourth, op::DRAW);
        // Two triangles are six vertices, counted from the primitive count so the
        // page never has to know the topology's arithmetic.
        assert_eq!(crate::render::payload_u32(payload, 2), Some(6));
        assert!(cursor.next().is_none());
        assert_eq!(host.d3d8.draws, 1);
    }

    /// Creates a device at 640x480 and answers the object, for the tests that
    /// need state to exist.
    fn device_at(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let parameters = crate::memory::STACK_BASE + 0x5100;
        let returned = crate::memory::STACK_BASE + 0x5200;
        memory.write_u32(parameters, 0).unwrap();
        memory.write_u32(parameters + 4, 0).unwrap();
        memory
            .write_u32(parameters + 8, D3DFMT_X8R8G8B8)
            .unwrap();
        let answer = invoke(
            host,
            memory,
            D3D8,
            "CreateDevice",
            &[0, 1, 0, 0x20, parameters, returned],
        )
        .expect("a device is created");
        assert_eq!(answer, Return(S_OK));
        memory.read_u32(returned).unwrap()
    }

    #[test]
    fn device_caps_are_written_where_a_guest_reads_them() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = device_at(&mut host, &mut memory);
        assert_eq!(device, host.d3d8.device_object);
        let caps = crate::memory::STACK_BASE + 0x6400;
        invoke(&mut host, &mut memory, D3D8_DEVICE, "GetDeviceCaps", &[caps])
            .expect("device caps");
        // D3DCAPS8 is a fixed sequence of members: DeviceType is first,
        // MaxTextureWidth the twenty-third, and MaxPixelShaderValue the last.
        assert_eq!(memory.read_u32(caps).unwrap(), 1, "DeviceType is HAL");
        assert_eq!(memory.read_u32(caps + 22 * 4).unwrap(), 4096, "MaxTextureWidth");
        assert_eq!(
            memory.read_u32(caps + 52 * 4).unwrap(),
            1.0_f32.to_bits(),
            "MaxPixelShaderValue is the last dword"
        );
    }

    #[test]
    fn a_vertex_buffer_lock_reports_the_range_the_guest_wrote() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let returned = crate::memory::STACK_BASE + 0x6100;
        invoke(
            &mut host,
            &mut memory,
            D3D8_DEVICE,
            "CreateVertexBuffer",
            &[64, 0, 0x142, 1, returned],
        )
        .expect("a vertex buffer");
        let buffer = memory.read_u32(returned).unwrap();
        let pointer_at = crate::memory::STACK_BASE + 0x6200;
        // Lock(offset 16, size 0, ppbData): a zero size means "to the end".
        invoke_on(
            &mut host,
            &mut memory,
            buffer,
            D3D8_VERTEX_BUFFER,
            "Lock",
            &[16, 0, pointer_at, 0],
        )
        .expect("a lock");
        let pointer = memory.read_u32(pointer_at).unwrap();
        let address = host.d3d8.vertex_buffers[&buffer].address;
        assert_eq!(pointer, address + 16);
        memory.write_bytes(pointer, &[0xab; 48]).unwrap();
        invoke_on(&mut host, &mut memory, buffer, D3D8_VERTEX_BUFFER, "Unlock", &[])
            .expect("an unlock");

        let mut cursor = crate::render::Cursor::new(host.d3d8.stream.bytes());
        let (first, _) = cursor.next().unwrap().unwrap();
        assert_eq!(first, op::CREATE_VERTEX_BUFFER);
        let (second, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(second, op::UPDATE_VERTEX_BUFFER);
        assert_eq!(crate::render::payload_u32(payload, 0), Some(buffer));
        assert_eq!(crate::render::payload_u32(payload, 1), Some(16));
        // The lock ran to the end of the 64-byte buffer.
        assert_eq!(crate::render::payload_u32(payload, 2), Some(48));
        // The page reads the vertices from linear memory by guest offset.
        assert_eq!(crate::render::payload_u32(payload, 3), Some(pointer - crate::pe::IMAGE_BASE));
    }

    #[test]
    fn a_texture_in_a_format_this_host_cannot_read_is_refused() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let returned = crate::memory::STACK_BASE + 0x6300;
        // A DXT1 texture: the page's upload has no decoder for it, and pretending
        // otherwise would draw whatever the compressed bytes look like as RGBA.
        let error = invoke(
            &mut host,
            &mut memory,
            D3D8_DEVICE,
            "CreateTexture",
            &[64, 64, 1, 0, 0x3154_5844, 1, returned],
        )
        .expect_err("a compressed texture is refused");
        assert!(error.contains("format"), "{error}");
    }

    #[test]
    fn copy_rects_moves_the_guest_bytes_a_rendered_frame_would() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = device_at(&mut host, &mut memory);
        let holder = crate::memory::STACK_BASE + 0x5300;
        // Two 8x8 A8R8G8B8 textures: the source a rendered frame would land in,
        // and the destination the next draw samples.
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "CreateTexture",
            &[8, 8, 1, 0, D3DFMT_A8R8G8B8, 0, holder],
        )
        .expect("a source texture");
        let source = memory.read_u32(holder).unwrap();
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "CreateTexture",
            &[8, 8, 1, 0, D3DFMT_A8R8G8B8, 0, holder],
        )
        .expect("a destination texture");
        let destination = memory.read_u32(holder).unwrap();

        // Every source pixel names its own coordinate, so a row that lands one
        // place off is a difference rather than the same colour over and over.
        let (address, pitch) = {
            let texture = &host.d3d8.textures[&source];
            (texture.address, texture.pitch)
        };
        for y in 0..8u32 {
            for x in 0..8u32 {
                memory
                    .write_u32(address + y * pitch + x * 4, y * 8 + x)
                    .unwrap();
            }
        }

        let source_holder = crate::memory::STACK_BASE + 0x5600;
        let destination_holder = crate::memory::STACK_BASE + 0x5700;
        invoke_on(
            &mut host,
            &mut memory,
            source,
            D3D8_TEXTURE,
            "GetSurfaceLevel",
            &[0, source_holder],
        )
        .expect("the source level's surface");
        invoke_on(
            &mut host,
            &mut memory,
            destination,
            D3D8_TEXTURE,
            "GetSurfaceLevel",
            &[0, destination_holder],
        )
        .expect("the destination level's surface");
        let source_surface = memory.read_u32(source_holder).unwrap();
        let destination_surface = memory.read_u32(destination_holder).unwrap();

        // `CopyRects(Source, Rects, 1, Destination, NULL)`: no points means each
        // rectangle lands at its own coordinates.
        let rect = crate::memory::STACK_BASE + 0x5800;
        for (offset, value) in [(0, 2), (4, 3), (8, 6), (12, 5)] {
            memory.write_u32(rect + offset, value).unwrap();
        }
        let answer = invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "CopyRects",
            &[source_surface, rect, 1, destination_surface, 0],
        )
        .expect("a copy");
        assert_eq!(answer, Return(S_OK));

        let target = host.d3d8.textures[&destination].address;
        let target_pitch = host.d3d8.textures[&destination].pitch;
        for y in 0..8u32 {
            for x in 0..8u32 {
                let got = memory.read_u32(target + y * target_pitch + x * 4).unwrap();
                let inside = (2..6).contains(&x) && (3..5).contains(&y);
                assert_eq!(
                    got,
                    if inside { y * 8 + x } else { 0 },
                    "pixel {x},{y} of the destination"
                );
            }
        }

        // The page is told to upload the level again, exactly as an unlock would
        // tell it: the bytes changed without a lock.
        let mut last = None;
        let mut cursor = crate::render::Cursor::new(host.d3d8.stream.bytes());
        while let Some(Ok((operation, payload))) = cursor.next() {
            last = Some((operation, crate::render::payload_u32(payload, 0)));
        }
        assert_eq!(last, Some((op::UPDATE_TEXTURE, Some(destination))));
    }

    #[test]
    fn set_render_target_takes_surface_handles_not_out_pointers() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = device_at(&mut host, &mut memory);
        let holder = crate::memory::STACK_BASE + 0x5300;
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "GetRenderTarget",
            &[holder],
        )
        .expect("a render target");
        let target = memory.read_u32(holder).unwrap();
        let depth_holder = crate::memory::STACK_BASE + 0x5400;
        invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "GetDepthStencilSurface",
            &[depth_holder],
        )
        .expect("a depth stencil");
        let depth = memory.read_u32(depth_holder).unwrap();
        let table = memory.read_u32(target).unwrap();

        // Both arguments are handles, so `SetRenderTarget` stores nothing
        // through them: neither object's vtable pointer moves. A save and a
        // restore around a render-to-texture pass is exactly this call twice.
        let answer = invoke_on(
            &mut host,
            &mut memory,
            device,
            D3D8_DEVICE,
            "SetRenderTarget",
            &[target, depth],
        )
        .expect("a target is set");
        assert_eq!(answer, Return(S_OK));
        assert_eq!(
            memory.read_u32(target).unwrap(),
            table,
            "the render target object is unchanged"
        );
        assert_eq!(
            invoke_on(
                &mut host,
                &mut memory,
                device,
                D3D8_DEVICE,
                "SetRenderTarget",
                &[0, 0]
            )
            .expect("the default target"),
            Return(S_OK)
        );
    }

    #[test]
    fn a_vertex_stride_comes_from_the_fvf() {
        // XYZ in 12 bytes, diffuse in 4, one two-float texture coordinate in 8.
        assert_eq!(fvf_stride(0x002 | 0x040 | 0x100), Some(24));
        // A transformed vertex is a float4, so XYZRHW plus diffuse is 20.
        assert_eq!(fvf_stride(0x004 | 0x040), Some(20));
        // Three-float texture coordinates come from the two size bits.
        assert_eq!(fvf_stride(0x002 | 0x100 | (1 << 16)), Some(24));
        // No position at all is not a vertex layout.
        assert_eq!(fvf_stride(0x040), None);
    }

    #[test]
    fn an_unknown_kind_is_not_mistaken_for_a_d3d8_interface() {
        assert!(!handles(crate::thunks::DDRAW));
        assert!(!handles(crate::thunks::DSOUND));
        // SetRenderState(this, state, value), from `d3d8.h`.
        assert_eq!(arg_bytes(D3D8_DEVICE, 50), 12);
        // A kind this module does not model gets a default rather than a panic;
        // `thunks` never routes one here.
        assert_eq!(arg_bytes(crate::thunks::DDRAW, 50), 4);
    }
}
