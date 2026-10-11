/*
 * C ABI between the Rust side of `scwc-capture` and the AVFoundation / VideoToolbox shim.
 *
 * Every function is called from the thread that owns the `CaptureSource`; the shim owns its
 * own capture and encoder threads and serializes them against these calls with locks. On
 * failure, a function returns a non-zero code and leaves a human-readable message in the
 * calling thread's slot, which `scwc_mac_last_error` copies out.
 */
#ifndef SCWC_MAC_SHIM_H
#define SCWC_MAC_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
  SCWC_MAC_OK = 0,
  SCWC_MAC_ERR_NO_CAMERA = 1,
  SCWC_MAC_ERR_UNSUPPORTED = 2,
  SCWC_MAC_ERR_PERMISSION = 3,
  SCWC_MAC_ERR_OS = 4,
  SCWC_MAC_ERR_BUSY = 5,
};

/* Capture request; the shim copies everything it needs before returning. */
typedef struct scwc_mac_config {
  /* NUL-terminated device uniqueID, or NULL for the system default camera. */
  const char *camera_id;
  uint32_t width;
  uint32_t height;
  uint32_t fps;
  /* 0 = H.264, 1 = H.265. */
  int32_t codec;
  uint32_t bitrate_kbps;
} scwc_mac_config;

/* One Annex B access unit with parameter sets in front of every keyframe. */
typedef void (*scwc_mac_frame_cb)(void *user, const uint8_t *data, size_t len,
                                  uint8_t is_keyframe);

/* One JPEG still; fired at most once per `scwc_mac_take_photo` call. */
typedef void (*scwc_mac_jpeg_cb)(void *user, const uint8_t *data, size_t len);

/* Start of one camera in the enumeration stream. */
typedef void (*scwc_mac_camera_begin_cb)(void *user, const char *id, const char *name);

/* One announced frame rate of the camera opened by the last begin callback. */
typedef void (*scwc_mac_camera_format_cb)(void *user, const char *resolution, uint32_t fps);

/* Lists every external and built-in video device; no session is created. */
int32_t scwc_mac_enumerate(scwc_mac_camera_begin_cb begin_cb,
                           scwc_mac_camera_format_cb format_cb, void *user);

/* Starts capture; writes the pipeline handle to `out_handle` on success. */
int32_t scwc_mac_open(const scwc_mac_config *cfg, scwc_mac_frame_cb frame_cb, void *user,
                      void **out_handle);

/* Rebuilds the session and the encoder for new parameters. */
int32_t scwc_mac_set_params(void *handle, uint32_t width, uint32_t height, uint32_t fps,
                            int32_t codec, uint32_t bitrate_kbps);

/* Encodes the latest captured frame as JPEG and hands it to `jpeg_cb` before returning. */
int32_t scwc_mac_take_photo(void *handle, scwc_mac_jpeg_cb jpeg_cb, void *user);

/* Stops the pipeline and frees the handle; no callback fires afterwards. */
void scwc_mac_close(void *handle);

/* Copies the calling thread's last failure message; returns its status code. */
int32_t scwc_mac_last_error(char *buf, size_t buf_len);

#ifdef __cplusplus
}
#endif
#endif /* SCWC_MAC_SHIM_H */
