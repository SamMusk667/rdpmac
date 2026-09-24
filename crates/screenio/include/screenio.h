/* screenio: screen capture, cursor state and input injection.
 * All functions are synchronous. They return SIO_OK or a negative SIO_E_* code. */
#ifndef SCREENIO_H
#define SCREENIO_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SIO_OK             0
#define SIO_E_TIMEOUT     -1  /* no new frame within the timeout */
#define SIO_E_RESET       -2  /* capture target changed, reopen the capturer */
#define SIO_E_PERMISSION  -3  /* missing OS permission */
#define SIO_E_UNSUPPORTED -4  /* not implemented on this platform */
#define SIO_E_INVALID     -5  /* bad argument */
#define SIO_E_OS          -6  /* OS call failed */
#define SIO_E_PANIC       -7  /* internal error */

#define SIO_FORMAT_BGRA 0

#define SIO_KEY_EXTENDED  1u  /* E0-prefixed scancode */
#define SIO_KEY_EXTENDED1 2u  /* E1-prefixed scancode */
#define SIO_KEY_RELEASE   4u  /* key release; absent means press */

#define SIO_LOCK_SCROLL   1u  /* lock key flags for sio_input_sync_locks, as in TS_SYNC_EVENT */
#define SIO_LOCK_NUM      2u
#define SIO_LOCK_CAPS     4u
#define SIO_LOCK_KANA     8u

#define SIO_BUTTON_LEFT   0u
#define SIO_BUTTON_RIGHT  1u
#define SIO_BUTTON_MIDDLE 2u
#define SIO_BUTTON_X1     3u
#define SIO_BUTTON_X2     4u

typedef struct sio_capture_t sio_capture_t;
typedef struct sio_input_t sio_input_t;

typedef struct sio_display_t {
    uint32_t id;
    int32_t x, y;            /* origin in virtual-desktop coordinates */
    uint32_t width, height;  /* size in captured pixels */
    float scale;             /* captured pixels per coordinate unit */
    uint8_t primary;
    char name[64];
} sio_display_t;

typedef struct sio_frame_t {
    const uint8_t *data;     /* valid until the next sio_capture_frame or sio_capture_close */
    uint32_t width, height;
    uint32_t stride;         /* bytes per row */
    uint32_t format;         /* SIO_FORMAT_* */
} sio_frame_t;

typedef struct sio_cursor_shape_t {
    uint64_t id;             /* changes whenever the shape changes */
    uint32_t width, height;
    int32_t hot_x, hot_y;
    uint8_t *rgba;           /* width * height * 4, release with sio_cursor_shape_free */
    size_t rgba_len;
    float scale;             /* bitmap pixels per point */
} sio_cursor_shape_t;

typedef struct sio_session_info_t {
    uint8_t can_capture;
    uint8_t can_inject;
    char backend[32];
} sio_session_info_t;

uint32_t    sio_version(void);
const char *sio_strerror(int err);

int  sio_display_list(sio_display_t *out, uint32_t cap, uint32_t *count);

int  sio_capture_open(uint32_t display_id, sio_capture_t **out);
int  sio_capture_open_scaled(uint32_t display_id, uint32_t width, uint32_t height, sio_capture_t **out);
int  sio_capture_frame(sio_capture_t *cap, uint32_t timeout_ms, sio_frame_t *out);
void sio_capture_close(sio_capture_t *cap);

int  sio_cursor_position(int32_t *x, int32_t *y, uint8_t *visible);
int  sio_cursor_shape(sio_cursor_shape_t *out);
void sio_cursor_shape_free(sio_cursor_shape_t *shape);

int  sio_input_open(sio_input_t **out);
int  sio_input_mouse_move(sio_input_t *in, int32_t x, int32_t y);
int  sio_input_mouse_move_rel(sio_input_t *in, int32_t dx, int32_t dy);
int  sio_input_mouse_button(sio_input_t *in, uint32_t button, uint8_t down);
int  sio_input_mouse_wheel(sio_input_t *in, int32_t dx, int32_t dy);   /* 120 per notch, + is up/right */
int  sio_input_key_scancode(sio_input_t *in, uint16_t set1_code, uint32_t flags);
int  sio_input_key_unicode(sio_input_t *in, uint32_t codepoint, uint8_t down);
int  sio_input_sync_locks(sio_input_t *in, uint32_t lock_flags);
int  sio_input_release_all(sio_input_t *in);
void sio_input_close(sio_input_t *in);

int  sio_session_info(sio_session_info_t *out);
int  sio_session_request_permissions(sio_session_info_t *out);

#ifdef __cplusplus
}
#endif
#endif /* SCREENIO_H */
