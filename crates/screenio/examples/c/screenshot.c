/* Lists displays, grabs one frame of the primary display into screenshot.ppm,
 * and prints the cursor state. Built by build.sh against libscreenio. */
#include <stdio.h>
#include <stdlib.h>
#include "screenio.h"

static int capture(uint32_t display_id) {
    sio_capture_t *cap = NULL;
    int rc = sio_capture_open(display_id, &cap);
    if (rc != SIO_OK) {
        fprintf(stderr, "sio_capture_open: %s\n", sio_strerror(rc));
        return 1;
    }
    sio_frame_t frame;
    rc = sio_capture_frame(cap, 3000, &frame);
    if (rc != SIO_OK) {
        fprintf(stderr, "sio_capture_frame: %s\n", sio_strerror(rc));
        sio_capture_close(cap);
        return 1;
    }
    FILE *f = fopen("screenshot.ppm", "wb");
    if (!f) {
        perror("screenshot.ppm");
        sio_capture_close(cap);
        return 1;
    }
    fprintf(f, "P6\n%u %u\n255\n", frame.width, frame.height);
    for (uint32_t y = 0; y < frame.height; y++) {
        const uint8_t *row = frame.data + (size_t)y * frame.stride;
        for (uint32_t x = 0; x < frame.width; x++) {
            const uint8_t *p = row + (size_t)x * 4;
            fputc(p[2], f); fputc(p[1], f); fputc(p[0], f);
        }
    }
    fclose(f);
    printf("wrote screenshot.ppm (%ux%u, stride %u)\n", frame.width, frame.height, frame.stride);
    sio_capture_close(cap);
    return 0;
}

int main(void) {
    sio_session_info_t info;
    sio_session_info(&info);
    printf("backend=%s can_capture=%d can_inject=%d\n", info.backend, info.can_capture, info.can_inject);

    sio_display_t displays[16];
    uint32_t count = 0;
    int rc = sio_display_list(displays, 16, &count);
    if (rc != SIO_OK) {
        fprintf(stderr, "sio_display_list: %s\n", sio_strerror(rc));
        return 1;
    }
    uint32_t target = 0;
    for (uint32_t i = 0; i < count && i < 16; i++) {
        printf("display %u: %s at (%d,%d) %ux%u scale %.1f primary %d\n", displays[i].id, displays[i].name,
               displays[i].x, displays[i].y, displays[i].width, displays[i].height, displays[i].scale,
               displays[i].primary);
        if (displays[i].primary) target = displays[i].id;
    }

    int status = capture(target);

    int32_t x, y;
    uint8_t visible;
    if (sio_cursor_position(&x, &y, &visible) == SIO_OK)
        printf("cursor at (%d,%d) visible=%d\n", x, y, visible);
    sio_cursor_shape_t shape;
    rc = sio_cursor_shape(&shape);
    if (rc == SIO_OK) {
        printf("cursor shape id %llu %ux%u hotspot (%d,%d)\n", (unsigned long long)shape.id, shape.width,
               shape.height, shape.hot_x, shape.hot_y);
        sio_cursor_shape_free(&shape);
    } else {
        printf("cursor shape: %s\n", sio_strerror(rc));
    }
    return status;
}
