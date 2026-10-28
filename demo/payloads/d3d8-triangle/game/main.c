// A Direct3D 8 guest: the renderer's own test pattern, and a payload.
//
// It exists so the 3D half of the kit has a program whose every call is
// deliberate: `Direct3DCreate8`, a device, a vertex buffer, a texture the guest
// fills through `LockRect`, and one draw per frame. A real game would tell you
// something is wrong by looking odd; this one tells you which call stopped
// drawing, because there is nothing else in it.
//
// What it exercises, in the order it runs:
//
//   d3d8     `Direct3DCreate8`, `CreateDevice`, the render and texture-stage
//            state, a world matrix per frame, a vertex buffer, a texture with
//            `LockRect`/`UnlockRect`, `SetStreamSource`, `DrawPrimitive`,
//            `Clear`, `BeginScene`/`EndScene`, `Present`
//   kernel32 `GetTickCount` for the animation, and `CreateFileA`/`ReadFile` for
//            the bitmap, so the guest's own file arrives through the file table
//   user32   `GetDesktopWindow`, which is all the focus window a device needs
//
// The picture is a rotating triangle with a 2x2 colour texture, which is the
// smallest thing that proves a transform, a texture, a colour and a depth buffer
// all reached the page.
//
// Build it 32-bit, fixed base, no relocations and no C runtime:
//
//   i686-w64-mingw32-gcc -m32 -O2 -nostdlib -ffreestanding -fno-builtin \
//     -fno-stack-protector -o triangle.exe main.c -ld3d8 -lkernel32 -luser32 \
//     -Wl,--disable-dynamicbase -Wl,--no-reloc -Wl,--image-base,0x400000 \
//     -Wl,--disable-nxcompat -Wl,--subsystem,windows -Wl,-e,start
//
// `-nostdlib` is deliberate: a mingw build that links its CRT imports
// `api-ms-win-crt-*`, which is a newer runtime than this kit models. Nothing
// here needs one -- `memset` and `memcpy` are the only two the compiler can emit
// a call to, and they are defined below.
//
// The kit maps a PE32 image at 0x400000 and never moves it, so the linker flags
// above are part of the contract, not a preference.

#define DIRECT3D_VERSION 0x0800
#define CINTERFACE
#define COBJMACROS

#include <windows.h>
#include <d3d8.h>

#define WIDTH 640
#define HEIGHT 480

/// What the vertex shader reads through `FVF`: position, colour, one coordinate.
typedef struct {
    float x, y, z;
    DWORD colour;
    float u, v;
} VERTEX;

#define FVF_TRIANGLE (D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX1)

/// The triangle, in clip space because the view and projection stay identity: a
/// `z` of 0.5 is inside Direct3D's 0..1 depth range, and the depth test then has
/// something to compare against the cleared 1.0.
static const VERTEX triangle[3] = {
    {-0.85f, -0.75f, 0.5f, 0xffff4040, 0.0f, 1.0f},
    {0.85f, -0.75f, 0.5f, 0xff40ff40, 1.0f, 1.0f},
    {0.00f, 0.85f, 0.5f, 0xff4040ff, 0.5f, 0.0f},
};

/// `cos(2*pi*i/32), sin(2*pi*i/32)`: a rotation table, because `sin` and `cos`
/// live in the C runtime this guest does not link.
static const float turn[64] = {
    +1.000000f, +0.000000f,
    +0.980785f, +0.195090f,
    +0.923880f, +0.382683f,
    +0.831470f, +0.555570f,
    +0.707107f, +0.707107f,
    +0.555570f, +0.831470f,
    +0.382683f, +0.923880f,
    +0.195090f, +0.980785f,
    +0.000000f, +1.000000f,
    -0.195090f, +0.980785f,
    -0.382683f, +0.923880f,
    -0.555570f, +0.831470f,
    -0.707107f, +0.707107f,
    -0.831470f, +0.555570f,
    -0.923880f, +0.382683f,
    -0.980785f, +0.195090f,
    -1.000000f, +0.000000f,
    -0.980785f, -0.195090f,
    -0.923880f, -0.382683f,
    -0.831470f, -0.555570f,
    -0.707107f, -0.707107f,
    -0.555570f, -0.831470f,
    -0.382683f, -0.923880f,
    -0.195090f, -0.980785f,
    +0.000000f, -1.000000f,
    +0.195090f, -0.980785f,
    +0.382683f, -0.923880f,
    +0.555570f, -0.831470f,
    +0.707107f, -0.707107f,
    +0.831470f, -0.555570f,
    +0.923880f, -0.382683f,
    +0.980785f, -0.195090f,
};

// ---- the C runtime's two compiler-generated helpers -----------------------

void *memset(void *destination, int value, unsigned count) {
    unsigned char *bytes = destination;
    for (unsigned index = 0; index < count; index += 1) bytes[index] = (unsigned char)value;
    return destination;
}

void *memcpy(void *destination, const void *source, unsigned count) {
    unsigned char *out = destination;
    const unsigned char *in = source;
    for (unsigned index = 0; index < count; index += 1) out[index] = in[index];
    return destination;
}

// ---- the bitmap ----------------------------------------------------------

/// A 24-bit BMP read straight from the guest's own file, into an A8R8G8B8
/// texture: Direct3D's 32-bit pixels are BGRA in memory, and a BMP row is BGR
/// bottom-up, so the copy reverses the rows and adds the alpha.
static IDirect3DTexture8 *load_texture(IDirect3DDevice8 *device) {
    static unsigned char bytes[4096];
    DWORD read = 0;
    HANDLE file = CreateFileA("triangle.bmp", GENERIC_READ, FILE_SHARE_READ, 0, OPEN_EXISTING, 0, 0);
    if (file == INVALID_HANDLE_VALUE) return 0;
    if (!ReadFile(file, bytes, sizeof(bytes), &read, 0) || read < 54) {
        CloseHandle(file);
        return 0;
    }
    CloseHandle(file);
    if (bytes[0] != 'B' || bytes[1] != 'M') return 0;

    DWORD offset = *(DWORD *)(bytes + 10);
    DWORD width = *(DWORD *)(bytes + 18);
    DWORD height = *(DWORD *)(bytes + 22);
    unsigned bits = *(unsigned short *)(bytes + 28);
    if (bits != 24 || width == 0 || height == 0 || width > 256 || height > 256) return 0;

    IDirect3DTexture8 *texture = 0;
    if (FAILED(IDirect3DDevice8_CreateTexture(
            device, width, height, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED, &texture)))
        return 0;

    D3DLOCKED_RECT locked;
    if (FAILED(IDirect3DTexture8_LockRect(texture, 0, &locked, 0, 0))) {
        IDirect3DTexture8_Release(texture);
        return 0;
    }
    unsigned stride = (width * 3 + 3) & ~3u;
    for (DWORD y = 0; y < height; y += 1) {
        const unsigned char *source = bytes + offset + (height - 1 - y) * stride;
        unsigned char *line = (unsigned char *)locked.pBits + y * locked.Pitch;
        for (DWORD x = 0; x < width; x += 1) {
            line[x * 4 + 0] = source[x * 3 + 0];
            line[x * 4 + 1] = source[x * 3 + 1];
            line[x * 4 + 2] = source[x * 3 + 2];
            line[x * 4 + 3] = 0xff;
        }
    }
    IDirect3DTexture8_UnlockRect(texture, 0);
    return texture;
}

// ---- the frame -----------------------------------------------------------

static void frame(IDirect3DDevice8 *device, IDirect3DVertexBuffer8 *buffer) {
    // One turn every 32 frames, from the clock rather than a counter so the speed
    // does not depend on how fast the host happens to be running.
    unsigned step = (GetTickCount() / 40) & 31;
    float cosine = turn[step * 2];
    float sine = turn[step * 2 + 1];
    // `D3DMATRIX` is row-major and Direct3D multiplies row vectors, so a rotation
    // about z puts the cosine on the diagonal and the sine off it. It is filled
    // through its first float rather than with a brace initialiser, because the
    // header declares it as a union of a struct and a `float[4][4]`.
    D3DMATRIX world;
    float *matrix = (float *)&world;
    matrix[0] = cosine;
    matrix[1] = sine;
    matrix[2] = 0.0f;
    matrix[3] = 0.0f;
    matrix[4] = -sine;
    matrix[5] = cosine;
    matrix[6] = 0.0f;
    matrix[7] = 0.0f;
    matrix[8] = 0.0f;
    matrix[9] = 0.0f;
    matrix[10] = 1.0f;
    matrix[11] = 0.0f;
    matrix[12] = 0.0f;
    matrix[13] = 0.0f;
    matrix[14] = 0.0f;
    matrix[15] = 1.0f;
    IDirect3DDevice8_SetTransform(device, D3DTS_WORLD, &world);

    IDirect3DDevice8_Clear(
        device, 0, 0, D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER, D3DCOLOR_XRGB(12, 18, 36), 1.0f, 0);

    if (SUCCEEDED(IDirect3DDevice8_BeginScene(device))) {
        IDirect3DDevice8_SetStreamSource(device, 0, buffer, sizeof(VERTEX));
        IDirect3DDevice8_DrawPrimitive(device, D3DPT_TRIANGLELIST, 0, 1);
        IDirect3DDevice8_EndScene(device);
    }
    IDirect3DDevice8_Present(device, 0, 0, 0, 0);
}

/// The image's entry point: there is no C runtime to call one for us.
void start(void) {
    IDirect3D8 *d3d = Direct3DCreate8(D3D_SDK_VERSION);
    if (d3d == 0) ExitProcess(1);

    D3DPRESENT_PARAMETERS present;
    memset(&present, 0, sizeof(present));
    present.Windowed = TRUE;
    present.SwapEffect = D3DSWAPEFFECT_DISCARD;
    // A 32-bit back buffer, because the page draws it through a WebGL2 context
    // that has no respect for a palettised one; the size is left to the display.
    present.BackBufferFormat = D3DFMT_X8R8G8B8;
    present.BackBufferCount = 1;
    present.EnableAutoDepthStencil = TRUE;
    present.AutoDepthStencilFormat = D3DFMT_D16;
    present.hDeviceWindow = GetDesktopWindow();

    IDirect3DDevice8 *device = 0;
    if (FAILED(IDirect3D8_CreateDevice(
            d3d,
            D3DADAPTER_DEFAULT,
            D3DDEVTYPE_HAL,
            present.hDeviceWindow,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING,
            &present,
            &device)))
        ExitProcess(2);

    IDirect3DVertexBuffer8 *buffer = 0;
    if (FAILED(IDirect3DDevice8_CreateVertexBuffer(
            device, sizeof(triangle), 0, FVF_TRIANGLE, D3DPOOL_MANAGED, &buffer)))
        ExitProcess(3);
    BYTE *vertices = 0;
    if (FAILED(IDirect3DVertexBuffer8_Lock(buffer, 0, 0, &vertices, 0))) ExitProcess(4);
    memcpy(vertices, triangle, sizeof(triangle));
    IDirect3DVertexBuffer8_Unlock(buffer);

    IDirect3DTexture8 *texture = load_texture(device);

    IDirect3DDevice8_SetRenderState(device, D3DRS_ZENABLE, TRUE);
    IDirect3DDevice8_SetRenderState(device, D3DRS_LIGHTING, FALSE);
    IDirect3DDevice8_SetRenderState(device, D3DRS_CULLMODE, D3DCULL_NONE);
    // Texture times vertex colour: the texture is the four quadrant colours and
    // the vertices are red, green and blue, so a wrong combiner is visible.
    IDirect3DDevice8_SetTextureStageState(device, 0, D3DTSS_COLOROP, D3DTOP_MODULATE);
    IDirect3DDevice8_SetTextureStageState(device, 0, D3DTSS_COLORARG1, D3DTA_TEXTURE);
    IDirect3DDevice8_SetTextureStageState(device, 0, D3DTSS_COLORARG2, D3DTA_DIFFUSE);
    IDirect3DDevice8_SetTexture(device, 0, (IDirect3DBaseTexture8 *)texture);
    IDirect3DDevice8_SetVertexShader(device, FVF_TRIANGLE);

    for (;;) frame(device, buffer);
}
