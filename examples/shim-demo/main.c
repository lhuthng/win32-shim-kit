// A shim demo that is also a small game: brick breaker at 640x480x8.
//
// It exists so the kit has one program whose every call is deliberate, and it
// targets exactly the interfaces `crates/host` models: `DirectDrawCreate`, so
// `IDirectDraw`/`IDirectDrawSurface`; `DirectSoundCreate`, so `IDirectSound` and
// `IDirectSoundBuffer`; `DirectInputCreateA` at version 5, so
// `IDirectInputDevice`. A newer interface (`DirectDrawCreateEx`, IDirectDraw7,
// DirectInput8) would be asking the host for a vtable it does not have.
//
//   ddraw    a 640x480x8 primary with a flipping chain and an 8-bit palette
//   dsound   one secondary buffer, played on every bounce
//   dinput   the keyboard, polled once per frame
//   winmm    timeGetTime, so the speed does not depend on the interpreter
//   user32   a window the guest owns, with a real message loop
//   kernel32 a high score on disk, so the file table is exercised
//
// Build it 32-bit, fixed base, no relocations:
//
//   i686-w64-mingw32-gcc -m32 -O2 -o demo.exe main.c \
//     -lddraw -ldsound -ldinput -lwinmm -luser32 -lgdi32 \
//     -Wl,--disable-dynamicbase -Wl,--no-reloc -Wl,--image-base,0x400000 \
//     -Wl,--disable-nxcompat -mwindows
//
// The kit maps a PE32 image at 0x400000 and never moves it, so the flags above
// are part of the contract, not a preference.

#define DIRECTDRAW_VERSION 0x0300
#define DIRECTSOUND_VERSION 0x0700
#define DIRECTINPUT_VERSION 0x0700

#include <windows.h>
#include <ddraw.h>
#include <dsound.h>
#include <dinput.h>
#include <stdio.h>

#define WIDTH 640
#define HEIGHT 480

#define BRICK_COLUMNS 10
#define BRICK_ROWS 5
#define BRICK_WIDTH (WIDTH / BRICK_COLUMNS)
#define BRICK_HEIGHT 16
#define BRICK_TOP 60

static LPDIRECTDRAW g_dd;
static LPDIRECTDRAWSURFACE g_primary;
static LPDIRECTDRAWSURFACE g_back;
static LPDIRECTDRAWPALETTE g_palette;
static LPDIRECTSOUND g_sound_device;
static LPDIRECTSOUNDBUFFER g_tone;
static LPDIRECTINPUT g_input;
static LPDIRECTINPUTDEVICE g_keyboard;
static HWND g_window;
static int g_running = 1;
static char g_error[256];

/* Sixteen shades of blue for the background, warm colours for the ball and the
 * bricks, and a ramp at the top for text. Index 0 is the page behind the game. */
static PALETTEENTRY g_palette_entries[256];

static void palette_init(void) {
    int index;
    for (index = 0; index < 256; index++) {
        int r = ((index >> 5) & 7) * 36;
        int g = ((index >> 2) & 7) * 36;
        int b = (index & 3) * 85;
        g_palette_entries[index].peRed = (BYTE)r;
        g_palette_entries[index].peGreen = (BYTE)g;
        g_palette_entries[index].peBlue = (BYTE)b;
        g_palette_entries[index].peFlags = 0;
    }
    /* The play area: a dark blue wash rather than the ramp's own dark corner. */
    for (index = 0; index < 16; index++) {
        g_palette_entries[index].peRed = (BYTE)(4 + index * 3);
        g_palette_entries[index].peGreen = (BYTE)(8 + index * 5);
        g_palette_entries[index].peBlue = (BYTE)(20 + index * 11);
    }
    g_palette_entries[0].peRed = 0;
    g_palette_entries[0].peGreen = 0;
    g_palette_entries[0].peBlue = 0;
}

static void fail(const char *what, HRESULT result) {
    if (FAILED(result)) {
        wsprintfA(g_error, "%s failed (%08lx)", what, (unsigned long)result);
        g_running = 0;
    }
}

static int window_open(HINSTANCE instance) {
    WNDCLASSA klass;
    RECT rect;
    memset(&klass, 0, sizeof klass);
    klass.lpfnWndProc = DefWindowProcA;
    klass.hInstance = instance;
    klass.lpszClassName = "ShimDemo";
    if (!RegisterClassA(&klass)) return 0;
    rect.left = 0;
    rect.top = 0;
    rect.right = WIDTH;
    rect.bottom = HEIGHT;
    AdjustWindowRect(&rect, WS_OVERLAPPEDWINDOW, FALSE);
    g_window = CreateWindowExA(0, "ShimDemo", "Shim demo: brick breaker",
        WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT,
        rect.right - rect.left, rect.bottom - rect.top, NULL, NULL, instance, NULL);
    if (!g_window) return 0;
    ShowWindow(g_window, SW_SHOW);
    return 1;
}

static int display_open(void) {
    DDSURFACEDESC desc;
    DDSCAPS caps;

    fail("DirectDrawCreate", DirectDrawCreate(NULL, &g_dd, NULL));
    if (!g_running) return 0;
    fail("SetCooperativeLevel", IDirectDraw_SetCooperativeLevel(g_dd, g_window, DDSCL_NORMAL));
    if (!g_running) return 0;
    fail("SetDisplayMode", IDirectDraw_SetDisplayMode(g_dd, WIDTH, HEIGHT, 8));
    if (!g_running) return 0;

    memset(&desc, 0, sizeof desc);
    desc.dwSize = sizeof desc;
    desc.dwFlags = DDSD_CAPS | DDSD_BACKBUFFERCOUNT;
    desc.ddsCaps.dwCaps = DDSCAPS_PRIMARYSURFACE | DDSCAPS_FLIP | DDSCAPS_COMPLEX;
    desc.dwBackBufferCount = 1;
    fail("CreateSurface(primary)", IDirectDraw_CreateSurface(g_dd, &desc, &g_primary, NULL));
    if (!g_running) return 0;

    memset(&caps, 0, sizeof caps);
    caps.dwCaps = DDSCAPS_BACKBUFFER;
    fail("GetAttachedSurface", IDirectDrawSurface_GetAttachedSurface(g_primary, &caps, &g_back));
    if (!g_running) return 0;

    fail("CreatePalette", IDirectDraw_CreatePalette(g_dd, DDPCAPS_8BIT | DDPCAPS_ALLOW256,
        g_palette_entries, &g_palette, NULL));
    if (!g_running) return 0;
    fail("SetPalette", IDirectDrawSurface_SetPalette(g_primary, g_palette));
    return g_running;
}

static int sound_open(void) {
    DSBUFFERDESC desc;
    WAVEFORMATEX format;
    unsigned char *bytes = NULL;
    DWORD written = 0;
    int index;

    fail("DirectSoundCreate", DirectSoundCreate(NULL, &g_sound_device, NULL));
    if (!g_running) return 0;
    fail("SetCooperativeLevel(dsound)", IDirectSound_SetCooperativeLevel(g_sound_device, g_window, DSSCL_NORMAL));
    if (!g_running) return 0;

    memset(&format, 0, sizeof format);
    format.wFormatTag = WAVE_FORMAT_PCM;
    format.nChannels = 1;
    format.nSamplesPerSec = 22050;
    format.wBitsPerSample = 8;
    format.nBlockAlign = 1;
    format.nAvgBytesPerSec = 22050;

    memset(&desc, 0, sizeof desc);
    desc.dwSize = sizeof desc;
    desc.dwFlags = DSBCAPS_CTRLDEFAULT;
    desc.dwBufferBytes = 2205; /* 100 ms */
    desc.lpwfxFormat = &format;
    fail("CreateSoundBuffer", IDirectSound_CreateSoundBuffer(g_sound_device, &desc, &g_tone, NULL));
    if (!g_running) return 0;

    if (SUCCEEDED(IDirectSoundBuffer_Lock(g_tone, 0, 0, (void **)&bytes, &written, NULL, NULL, DSBLOCK_ENTIREBUFFER))) {
        for (index = 0; index < (int)written; index++) {
            int edge = index < 200 || index > (int)written - 200;
            bytes[index] = edge ? 128 : (unsigned char)(((index / 25) % 2) ? 200 : 56);
        }
        IDirectSoundBuffer_Unlock(g_tone, bytes, written, NULL, 0);
    }
    return g_running;
}

static void sound_play(void) {
    if (!g_tone) return;
    IDirectSoundBuffer_SetCurrentPosition(g_tone, 0);
    IDirectSoundBuffer_Play(g_tone, 0, 0, 0);
}

static int input_open(HINSTANCE instance) {
    fail("DirectInputCreateA", DirectInputCreateA(instance, DIRECTINPUT_VERSION, &g_input, NULL));
    if (!g_running) return 0;
    fail("CreateDevice(keyboard)", IDirectInput_CreateDevice(g_input, &GUID_SysKeyboard, &g_keyboard, NULL));
    if (!g_running) return 0;
    fail("SetDataFormat", IDirectInputDevice_SetDataFormat(g_keyboard, &c_dfDIKeyboard));
    if (!g_running) return 0;
    fail("SetCooperativeLevel(dinput)", IDirectInputDevice_SetCooperativeLevel(g_keyboard, g_window, DISCL_FOREGROUND | DISCL_NONEXCLUSIVE));
    if (!g_running) return 0;
    IDirectInputDevice_Acquire(g_keyboard);
    return g_running;
}

/* ---- the game ---------------------------------------------------------- */

static int g_bricks[BRICK_ROWS][BRICK_COLUMNS];
static int g_bricks_left;
static float g_paddle, g_ball_x, g_ball_y, g_ball_vx, g_ball_vy;
static int g_score, g_high;

static void screen_clear(unsigned char *pixels, int pitch, unsigned char colour) {
    int y;
    for (y = 0; y < HEIGHT; y++) {
        memset(pixels + y * pitch, colour, WIDTH);
    }
}

static void fill(unsigned char *pixels, int pitch, int x, int y, int w, int h, unsigned char colour) {
    int row, column;
    for (row = y; row < y + h; row++) {
        if (row < 0 || row >= HEIGHT) continue;
        for (column = x; column < x + w; column++) {
            if (column < 0 || column >= WIDTH) continue;
            pixels[row * pitch + column] = colour;
        }
    }
}

/* A 5x7 bitmap font: digits, capitals and a few marks, enough for a score. */
static const unsigned char kFont[][5] = {
    {0x3E, 0x51, 0x49, 0x45, 0x3E}, {0x00, 0x42, 0x7F, 0x40, 0x00},
    {0x72, 0x49, 0x49, 0x49, 0x46}, {0x21, 0x41, 0x49, 0x4D, 0x33},
    {0x18, 0x14, 0x12, 0x7F, 0x10}, {0x27, 0x45, 0x45, 0x45, 0x39},
    {0x3C, 0x4A, 0x49, 0x49, 0x31}, {0x41, 0x21, 0x11, 0x09, 0x07},
    {0x36, 0x49, 0x49, 0x49, 0x36}, {0x46, 0x49, 0x49, 0x29, 0x1E},
    {0x7F, 0x09, 0x09, 0x09, 0x06}, {0x3E, 0x41, 0x41, 0x41, 0x22},
    {0x1C, 0x22, 0x41, 0x41, 0x41}, {0x7F, 0x41, 0x41, 0x22, 0x1C},
    {0x7F, 0x49, 0x49, 0x49, 0x41}, {0x7F, 0x09, 0x09, 0x01, 0x01},
    {0x3E, 0x41, 0x49, 0x49, 0x7A}, {0x7F, 0x08, 0x08, 0x08, 0x7F},
    {0x00, 0x41, 0x7F, 0x41, 0x00}, {0x20, 0x40, 0x41, 0x3F, 0x01},
    {0x7F, 0x08, 0x14, 0x22, 0x41}, {0x7F, 0x40, 0x40, 0x40, 0x40},
    {0x7F, 0x02, 0x0C, 0x02, 0x7F}, {0x7F, 0x04, 0x08, 0x10, 0x7F},
    {0x3E, 0x41, 0x41, 0x41, 0x3E}, {0x7F, 0x09, 0x09, 0x09, 0x06},
    {0x3E, 0x41, 0x51, 0x21, 0x5E}, {0x7F, 0x09, 0x19, 0x29, 0x46},
    {0x46, 0x49, 0x49, 0x49, 0x31}, {0x01, 0x01, 0x7F, 0x01, 0x01},
    {0x3F, 0x40, 0x40, 0x40, 0x3F}, {0x1F, 0x20, 0x40, 0x20, 0x1F},
    {0x3F, 0x40, 0x38, 0x40, 0x3F}, {0x63, 0x14, 0x08, 0x14, 0x63},
    {0x07, 0x08, 0x70, 0x08, 0x07}, {0x61, 0x51, 0x49, 0x45, 0x43},
    {0x00, 0x00, 0x00, 0x00, 0x00}, /* space */
};

static int glyph_index(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'A' && c <= 'Z') return 10 + (c - 'A');
    if (c >= 'a' && c <= 'z') return 10 + (c - 'a');
    return 36;
}

static void text(unsigned char *pixels, int pitch, int x, int y, const char *message, unsigned char colour, int scale) {
    for (; *message; message++) {
        int glyph = glyph_index(*message), column, row;
        for (column = 0; column < 5; column++) {
            for (row = 0; row < 7; row++) {
                if (kFont[glyph][column] & (1 << row)) {
                    fill(pixels, pitch, x + column * scale, y + row * scale, scale, scale, colour);
                }
            }
        }
        x += 6 * scale;
    }
}

static void game_reset(void) {
    int row, column;
    g_bricks_left = 0;
    for (row = 0; row < BRICK_ROWS; row++) {
        for (column = 0; column < BRICK_COLUMNS; column++) {
            g_bricks[row][column] = 1;
            g_bricks_left++;
        }
    }
    g_paddle = WIDTH / 2.0f - 40;
    g_ball_x = WIDTH / 2.0f;
    g_ball_y = HEIGHT - 80;
    g_ball_vx = 3.4f;
    g_ball_vy = -3.8f;
    g_score = 0;
}

static void high_load(void) {
    FILE *file = fopen("DEMO.SCO", "rb");
    g_high = 0;
    if (!file) return;
    if (fread(&g_high, sizeof g_high, 1, file) != 1) g_high = 0;
    fclose(file);
}

static void high_save(void) {
    FILE *file;
    if (g_score <= g_high) return;
    g_high = g_score;
    file = fopen("DEMO.SCO", "wb");
    if (!file) return;
    fwrite(&g_high, sizeof g_high, 1, file);
    fclose(file);
}

static void step(float dt) {
    static unsigned char keys[256];
    int move = 0;

    if (g_keyboard) {
        if (FAILED(IDirectInputDevice_GetDeviceState(g_keyboard, 256, keys))) {
            IDirectInputDevice_Acquire(g_keyboard);
            memset(keys, 0, sizeof keys);
        }
        if (keys[DIK_ESCAPE] & 0x80) g_running = 0;
        if (keys[DIK_LEFT] & 0x80 || keys[DIK_A] & 0x80) move = -1;
        if (keys[DIK_RIGHT] & 0x80 || keys[DIK_D] & 0x80) move = 1;
    }

    g_paddle += move * 460.0f * dt;
    if (g_paddle < 0) g_paddle = 0;
    if (g_paddle > WIDTH - 80) g_paddle = WIDTH - 80;

    g_ball_x += g_ball_vx * dt * 60.0f;
    g_ball_y += g_ball_vy * dt * 60.0f;

    if (g_ball_x < 4) { g_ball_x = 4; g_ball_vx = -g_ball_vx; sound_play(); }
    if (g_ball_x > WIDTH - 8) { g_ball_x = WIDTH - 8; g_ball_vx = -g_ball_vx; sound_play(); }
    if (g_ball_y < 4) { g_ball_y = 4; g_ball_vy = -g_ball_vy; sound_play(); }

    if (g_ball_y > HEIGHT - 34 && g_ball_y < HEIGHT - 22 &&
        g_ball_x > g_paddle && g_ball_x < g_paddle + 80 && g_ball_vy > 0) {
        g_ball_vy = -g_ball_vy;
        g_ball_vx += (g_ball_x - (g_paddle + 40)) * 0.06f;
        sound_play();
    }

    if (g_ball_y > HEIGHT) {
        high_save();
        game_reset();
    }

    {
        int column = (int)(g_ball_x / BRICK_WIDTH);
        int row = (int)((g_ball_y - BRICK_TOP) / BRICK_HEIGHT);
        if (row >= 0 && row < BRICK_ROWS && column >= 0 && column < BRICK_COLUMNS && g_bricks[row][column]) {
            g_bricks[row][column] = 0;
            g_bricks_left--;
            g_score += 10;
            g_ball_vy = -g_ball_vy;
            sound_play();
            if (g_bricks_left == 0) {
                high_save();
                game_reset();
            }
        }
    }
}

static void draw(unsigned char *pixels, int pitch) {
    int row, column;
    char line[64];

    screen_clear(pixels, pitch, 1);
    fill(pixels, pitch, 0, 0, WIDTH, 4, 6);
    fill(pixels, pitch, 0, HEIGHT - 4, WIDTH, 4, 6);

    for (row = 0; row < BRICK_ROWS; row++) {
        for (column = 0; column < BRICK_COLUMNS; column++) {
            if (!g_bricks[row][column]) continue;
            fill(pixels, pitch, column * BRICK_WIDTH + 2, BRICK_TOP + row * BRICK_HEIGHT + 2,
                BRICK_WIDTH - 4, BRICK_HEIGHT - 4, (unsigned char)(40 + row * 30));
        }
    }

    fill(pixels, pitch, (int)g_paddle, HEIGHT - 30, 80, 8, 200);
    fill(pixels, pitch, (int)g_ball_x - 3, (int)g_ball_y - 3, 7, 7, 255);

    snprintf(line, sizeof line, "SCORE %d", g_score);
    text(pixels, pitch, 12, 12, line, 255, 2);
    snprintf(line, sizeof line, "HIGH %d", g_high);
    text(pixels, pitch, WIDTH - 12 - 11 * 12, 12, line, 160, 2);
    text(pixels, pitch, 12, HEIGHT - 20, "ARROWS MOVE   ESC QUIT", 96, 1);
}

static void present(void) {
    DDSURFACEDESC desc;
    HRESULT result;
    int attempts = 0;

    memset(&desc, 0, sizeof desc);
    desc.dwSize = sizeof desc;
    result = IDirectDrawSurface_Lock(g_back, NULL, &desc, DDLOCK_WAIT, NULL);
    if (FAILED(result)) {
        if (result == DDERR_SURFACELOST) {
            IDirectDrawSurface_Restore(g_primary);
            IDirectDrawSurface_Restore(g_back);
        }
        return;
    }
    draw((unsigned char *)desc.lpSurface, (int)desc.lPitch);
    IDirectDrawSurface_Unlock(g_back, NULL);

    while (attempts++ < 4) {
        result = IDirectDrawSurface_Flip(g_back, NULL, DDFLIP_WAIT);
        if (SUCCEEDED(result)) return;
        if (result == DDERR_SURFACELOST) {
            IDirectDrawSurface_Restore(g_primary);
            IDirectDrawSurface_Restore(g_back);
        } else {
            return;
        }
    }
}

int WINAPI WinMain(HINSTANCE instance, HINSTANCE previous, LPSTR command, int show) {
    DWORD previous_time;
    MSG message;

    (void)previous;
    (void)command;
    (void)show;

    palette_init();
    high_load();

    if (!window_open(instance)) return 1;
    if (!display_open()) {
        MessageBoxA(g_window, g_error, "Shim demo", MB_OK | MB_ICONERROR);
        return 1;
    }
    sound_open();
    input_open(instance);
    game_reset();

    previous_time = timeGetTime();
    while (g_running) {
        DWORD now;
        float dt;
        while (PeekMessageA(&message, NULL, 0, 0, PM_REMOVE)) {
            if (message.message == WM_QUIT) g_running = 0;
            TranslateMessage(&message);
            DispatchMessageA(&message);
        }
        now = timeGetTime();
        dt = (now - previous_time) / 1000.0f;
        if (dt > 0.1f) dt = 0.1f;
        previous_time = now;
        if (dt > 0.0f) step(dt);
        present();
        Sleep(10);
    }

    high_save();
    if (g_keyboard) { IDirectInputDevice_Unacquire(g_keyboard); IDirectInputDevice_Release(g_keyboard); }
    if (g_input) IDirectInput_Release(g_input);
    if (g_tone) IDirectSoundBuffer_Release(g_tone);
    if (g_sound_device) IDirectSound_Release(g_sound_device);
    if (g_palette) IDirectDrawPalette_Release(g_palette);
    if (g_back) IDirectDrawSurface_Release(g_back);
    if (g_primary) IDirectDrawSurface_Release(g_primary);
    if (g_dd) IDirectDraw_Release(g_dd);
    return 0;
}
