/*
 * C ABI between the Rust side of `scwc-capture` and the AVFoundation / VideoToolbox shim.
 *
 * Every function is called from the thread that owns the `CaptureSource`; the shim owns its
 * own capture and encoder threads and serializes them against these calls with locks. The
 * callbacks are invoked on those threads, except the status callback, which runs on whichever
 * thread reported the change: the one that posted the platform's notification, the delegate
 * queue of a frame the encoder refused, or a VideoToolbox thread. On failure, a function
 * returns a non-zero code and leaves a human-readable message in the calling thread's slot,
 * which `scwc_mac_last_error` copies out.
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

/* One live preview frame, tightly packed BGRA (blue, green, red, alpha, one byte each, no row
 * padding): `width * height` pixels at `data`, `len` bytes long. The size is the one the gate
 * asked the shim to render at, never the captured frame, and the callback is fired off the
 * capture queue, after the capture callback that produced the frame has long returned. The
 * callback is expected to copy what it needs before returning: the buffer belongs to the shim. */
typedef void (*scwc_mac_preview_cb)(void *user, const uint8_t *data, size_t len, uint32_t width,
                                    uint32_t height);

/* Where the next preview frame should be rendered, if one is wanted at all: `frame_width` and
 * `frame_height` are the captured frame, and a non-zero return writes the size to render it at
 * into `out_width`/`out_height` (both non-zero — the frame's aspect ratio fitted inside the size
 * the interface asked for, so it is never the capture resolution).
 *
 * Zero means no frame: nothing is registered, the interface shows no preview, or the consumer has
 * not finished with the frame it was handed. The shim then renders nothing at all, which is what
 * keeps a preview nobody can keep up with free. Called on the capture's own queue and must return
 * promptly: it must not convert anything. */
typedef int32_t (*scwc_mac_preview_wanted_cb)(void *user, uint32_t frame_width,
                                              uint32_t frame_height, uint32_t *out_width,
                                              uint32_t *out_height);

/* Device status change; `state` is a thermal state — 0 nominal, 1 fair, 2 serious, 3 critical
 * — or SCWC_MAC_STATUS_FAILURE, which reports that the pipeline hit a hard failure and that
 * capture has probably stopped. Thermal states are iOS-only: the shim subscribes to
 * `NSProcessInfoThermalStateDidChangeNotification` there. */
typedef void (*scwc_mac_status_cb)(void *user, int32_t state);

/* The status a pipeline reports once when it fails: an encoder that stopped encoding, a
 * session that died at runtime. The platform's own description of the failure is written to
 * the platform log by the shim, next to this report. */
enum {
  SCWC_MAC_STATUS_FAILURE = -1,
};

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

/* Registers the device status callback, or unregisters it when `cb` is NULL. The callback
 * stays registered until it is replaced, so a `scwc_mac_set_params` reconfigure keeps it. */
int32_t scwc_mac_set_status_cb(void *handle, scwc_mac_status_cb cb, void *user);

/* One captured frame handed over as the platform's own buffer, for a consumer that renders it
 * without a conversion: `buffer` is a `CVPixelBufferRef` lent for the duration of the call, in
 * the camera's own size. A callee that keeps it past the return retains it with
 * `scwc_mac_retain_buffer` and releases that reference when it is done, so no CoreVideo type has
 * to be known outside this shim. Fired for every captured frame while a callback is registered,
 * on the capture queue, and the callee must return promptly: whatever it does with the buffer
 * belongs on its own thread. */
typedef void (*scwc_mac_frame_slot_cb)(void *user, void *buffer);

/* Registers the frame-slot callback, or unregisters it when `cb` is NULL. Like the preview
 * callback, it survives a `scwc_mac_set_params` reconfigure. */
int32_t scwc_mac_set_frame_slot_cb(void *handle, scwc_mac_frame_slot_cb cb, void *user);

/* Retains a buffer the frame-slot callback lent and hands it back, so a consumer can own the
 * frame it keeps without CoreVideo: the returned reference is released by the caller through
 * `scwc_mac_release_buffer`. */
void *scwc_mac_retain_buffer(void *buffer);

/* Releases one reference to a buffer retained by `scwc_mac_retain_buffer`. */
void scwc_mac_release_buffer(void *buffer);

/* Turns the hardware encoder on or off. While it is off the delegate only publishes preview
 * frames, and the encoder is not fed, so a camera with no active stream costs no encoding at all.
 * Turning it on forces the first frame after the call to be a keyframe, so a recording always
 * starts on an IDR. */
int32_t scwc_mac_set_encoding(void *handle, int32_t enabled);

/* Registers the preview callback and its gate, or unregisters both when `cb` is NULL. Like the
 * status callback, it survives a `scwc_mac_set_params` reconfigure: a parameter switch is not a
 * reason to re-register the preview. */
int32_t scwc_mac_set_preview_cb(void *handle, scwc_mac_preview_cb cb, void *user,
                                scwc_mac_preview_wanted_cb wanted);

/* Rebuilds the session and the encoder for new parameters. A failure leaves the pipeline that
 * is running — and the frames it delivers — in place, on the parameters it already had. */
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
