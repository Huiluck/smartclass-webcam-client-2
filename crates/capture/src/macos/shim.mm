//
// Objective-C++ shim: AVFoundation capture and VideoToolbox encoding for `scwc-capture`.
//
// One shim serves both Apple platforms, because AVFoundation and VideoToolbox are the same
// APIs on macOS and iOS: what differs — device discovery, session presets, the interruptions
// and thermal pressure an iOS app has to live with — sits behind `TARGET_OS_IPHONE`.
//
// The shim owns every platform object behind an opaque handle. Rust never sees an
// Objective-C type: it calls the entry points at the bottom of this file and receives
// Annex B access units and JPEG stills through the callbacks it registers.
//
// Layout note: the class is declared first so the helpers below can message it, and the
// helpers come before the implementation that uses them.
//

// `TARGET_OS_IPHONE` splits the two platforms in this file; what is macOS-only sits in its
// `#else`, so the macOS path is what it has always been.
#include <TargetConditionals.h>

#import <AVFoundation/AVFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreImage/CoreImage.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>
#import <VideoToolbox/VideoToolbox.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <functional>
#include <mutex>
#include <string>
#include <vector>

#include "shim.h"

/// A live capture pipeline: session, encoder and the state shared with their callbacks.
@interface ScwcMacCapture : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate> {
  dispatch_queue_t _delegateQueue;
  AVCaptureSession *_session;
  AVCaptureVideoDataOutput *_output;
  AVCaptureDevice *_device;
  AVCaptureDeviceFormat *_format;
  VTCompressionSessionRef _encoder;
  CVPixelBufferRef _latestPixelBuffer;
  std::mutex _photoLock;
  std::mutex _photoInFlight;
  std::mutex _callbackLock;
  /// Guards `_session`. The iOS notification blocks below run on the thread that posted the
  /// notification, which is not the thread that tears the pipeline down; everything that
  /// touches `_session` from there goes through this lock.
  std::mutex _sessionLock;
  scwc_mac_frame_cb _frameCallback;
  void *_frameUser;
  scwc_mac_status_cb _statusCallback;
  void *_statusUser;
  /// Preview state, guarded by `_previewLock`: the callback and its gate, and the tight BGRA
  /// buffer frames are converted into. The lock is held across a conversion, so a registration
  /// and a frame being converted cannot interleave; nothing else is held while it is, and the
  /// Rust side the conversion hands the frame to never re-enters this object.
  scwc_mac_preview_cb _previewCallback;
  void *_previewUser;
  scwc_mac_preview_wanted_cb _previewWanted;
  std::mutex _previewLock;
  std::vector<uint8_t> _previewPixels;
  int32_t _codec;
  uint32_t _width;
  uint32_t _height;
  uint32_t _fps;
  uint32_t _bitrateKbps;
  std::string _cameraId;
  bool _shutDown;
  /// Whether the pipeline has already reported a hard failure: an encoder that is dead stays
  /// dead, so the operator gets one report instead of one per frame.
  bool _failureReported;
  /// Notification tokens, kept so `teardown` can cancel the subscriptions.
  id _runtimeErrorObserver;
  id _interruptionObserver;
#if TARGET_OS_IPHONE
  id _thermalObserver;
  id _interruptionEndedObserver;
  /// The last thermal state reported to Rust, so only transitions are logged.
  int32_t _lastThermalState;
#endif
}
- (int32_t)startWithConfig:(const scwc_mac_config *)cfg
              frameCallback:(scwc_mac_frame_cb)callback
                       user:(void *)user;
- (int32_t)setWidth:(uint32_t)width
             height:(uint32_t)height
                fps:(uint32_t)fps
              codec:(int32_t)codec
        bitrateKbps:(uint32_t)bitrate;
- (int32_t)takePhotoWithCallback:(scwc_mac_jpeg_cb)callback user:(void *)user;
- (void)shutdown;
/// Publishes the device status callback `scwc_mac_set_status_cb` registers.
- (void)setStatusCallback:(scwc_mac_status_cb)callback user:(void *)user;
/// Publishes the preview callback `scwc_mac_set_preview_cb` registers, and the gate it is
/// consulted through.
- (void)setPreviewCallback:(scwc_mac_preview_cb)callback
                      user:(void *)user
                    wanted:(scwc_mac_preview_wanted_cb)wanted;
/// Converts one captured frame to BGRA and hands it to Rust, unless nothing wants it.
- (void)publishPreview:(CVPixelBufferRef)pixel_buffer;
/// Subscribes to the notifications that report a failed session and thermal pressure.
- (void)subscribeNotifications;
/// Cancels those subscriptions; afterwards no notification block reaches this object again.
- (void)unsubscribeNotifications;
#if TARGET_OS_IPHONE
/// Logs one thermal transition and forwards it to Rust.
- (void)reportThermalState:(int32_t)state;
/// Restarts the session an interruption ended on.
- (void)resumeAfterInterruption:(AVCaptureSession *)session;
#endif
/// Flattens one encoded sample into an Annex B access unit and hands it to Rust.
- (void)handleEncodedSample:(CMSampleBufferRef)sample status:(OSStatus)status;
@end

namespace {

/// One set of capture parameters, as a start or a reconfiguration receives them.
struct CaptureParameters {
  uint32_t width;
  uint32_t height;
  uint32_t fps;
  int32_t codec;
  uint32_t bitrate_kbps;
};

/// Failure reported by the last call on this thread.
thread_local int32_t gLastCode = SCWC_MAC_OK;
thread_local std::string gLastMessage;

/// Records a failure and returns its code, so call sites read `return fail(...)`.
int32_t fail(int32_t code, NSString *message) {
  gLastCode = code;
  gLastMessage = message != nil ? std::string(message.UTF8String) : std::string();
  return code;
}

/// Copies a NUL-terminated C string into a `std::string`.
std::string from_c_string(const char *value) {
  return value != nullptr ? std::string(value) : std::string();
}

/// Applies one encoder property, logging the ones the codec does not take.
void set_property(VTCompressionSessionRef session, CFStringRef key, CFTypeRef value) {
  OSStatus status = VTSessionSetProperty(session, key, value);
  if (status != noErr) {
    NSLog(@"[scwc-capture] encoder property %@ rejected: %d", key, (int)status);
  }
}

/// Highest frame rate a single advertised range can deliver.
///
/// `maxFrameRate` and `1 / minFrameDuration` describe the same end of the range; the
/// duration is the exact one, the float is the rounded one, so the duration wins.
double range_max_fps(AVFrameRateRange *range) {
  if (CMTIME_IS_VALID(range.minFrameDuration)) {
    double seconds = CMTimeGetSeconds(range.minFrameDuration);
    if (seconds > 0 && std::isfinite(seconds)) {
      return 1.0 / seconds;
    }
  }
  return range.maxFrameRate;
}

/// Highest frame rate any format range of the device advertises.
double format_max_fps(AVCaptureDeviceFormat *format) {
  double best = 0.0;
  for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
    double fps = range_max_fps(range);
    if (std::isfinite(fps) && fps > best) {
      best = fps;
    }
  }
  return best;
}

/// Whether the format has exactly the requested dimensions.
bool format_matches(AVCaptureDeviceFormat *format, uint32_t width, uint32_t height) {
  CMFormatDescriptionRef description = format.formatDescription;
  if (CMFormatDescriptionGetMediaType(description) != kCMMediaType_Video) {
    return false;
  }
  CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(description);
  return (uint32_t)dimensions.width == width && (uint32_t)dimensions.height == height;
}

/// The first format with the requested dimensions that reaches the requested rate.
///
/// iPhone 8 and later advertise 3840x2160@60 among their formats, so 4K60 needs no special
/// case on iOS: this same search finds it. (Unverified on hardware — the iOS simulator has
/// no camera — as is the rest of the iOS path.)
AVCaptureDeviceFormat *pick_format(AVCaptureDevice *device, uint32_t width, uint32_t height,
                                   uint32_t fps) {
  for (AVCaptureDeviceFormat *format in device.formats) {
    if (!format_matches(format, width, height)) {
      continue;
    }
    // The advertised rates are rounded, so allow half a frame of slack.
    if (format_max_fps(format) + 0.5 >= (double)fps) {
      return format;
    }
  }
  return nil;
}

/// A frame duration for `fps` that the format actually accepts.
///
/// AVFoundation raises an exception for a duration outside the active format's ranges, so
/// the value is taken from a range that admits the request, or clamped into the fastest one.
CMTime frame_duration(AVCaptureDeviceFormat *format, uint32_t fps) {
  CMTime want = CMTimeMake(1, (int32_t)fps);
  CMTime fastest = kCMTimeInvalid;
  CMTime slowest = kCMTimeInvalid;

  for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
    if (CMTimeCompare(want, range.minFrameDuration) >= 0 &&
        CMTimeCompare(want, range.maxFrameDuration) <= 0) {
      return want;
    }
    if (!CMTIME_IS_VALID(fastest) || CMTimeCompare(range.minFrameDuration, fastest) < 0) {
      fastest = range.minFrameDuration;
    }
    if (!CMTIME_IS_VALID(slowest) || CMTimeCompare(range.maxFrameDuration, slowest) > 0) {
      slowest = range.maxFrameDuration;
    }
  }

  if (CMTIME_IS_VALID(fastest) && CMTimeCompare(want, fastest) < 0) {
    want = fastest;
  }
  if (CMTIME_IS_VALID(slowest) && CMTimeCompare(want, slowest) > 0) {
    want = slowest;
  }
  return want;
}

/// Every video device of the host, built-in and external.
NSArray<AVCaptureDevice *> *video_devices() {
  NSMutableArray<AVCaptureDeviceType> *types = [NSMutableArray array];
#if TARGET_OS_IPHONE
  // iOS exposes the camera cluster as separate devices: the physical cameras an iPhone
  // carries, of which a given model has one to three. Asking for a type the device does not
  // have simply yields no device for it.
  [types addObject:AVCaptureDeviceTypeBuiltInWideAngleCamera];
  if (@available(iOS 13.0, *)) {
    [types addObject:AVCaptureDeviceTypeBuiltInUltraWideCamera];
  }
  [types addObject:AVCaptureDeviceTypeBuiltInTelephotoCamera];
#else
  [types addObject:AVCaptureDeviceTypeBuiltInWideAngleCamera];
#if defined(__MAC_OS_X_VERSION_MAX_ALLOWED) && __MAC_OS_X_VERSION_MAX_ALLOWED >= 140000
  if (@available(macOS 14.0, *)) {
    [types addObject:AVCaptureDeviceTypeExternal];
  } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    [types addObject:AVCaptureDeviceTypeExternalUnknown];
#pragma clang diagnostic pop
  }
#else
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
  [types addObject:AVCaptureDeviceTypeExternalUnknown];
#pragma clang diagnostic pop
#endif
#endif

  AVCaptureDeviceDiscoverySession *session = [AVCaptureDeviceDiscoverySession
      discoverySessionWithDeviceTypes:types
                            mediaType:AVMediaTypeVideo
                             position:AVCaptureDevicePositionUnspecified];
  return session.devices;
}

/// The name this shim reports for `device`.
///
/// macOS names its cameras after the hardware. iOS calls every built-in camera "Camera", so
/// the position is what tells them apart in a camera list; it goes into the name here rather
/// than into a field of its own, because the protocol's camera list is just (id, name).
NSString *device_name(AVCaptureDevice *device) {
#if TARGET_OS_IPHONE
  NSString *position = @"Unspecified";
  switch (device.position) {
    case AVCaptureDevicePositionFront:
      position = @"Front";
      break;
    case AVCaptureDevicePositionBack:
      position = @"Back";
      break;
    default:
      break;
  }
  return [NSString stringWithFormat:@"%@ (%@)", device.localizedName, position];
#else
  return device.localizedName;
#endif
}

/// The camera the system picks when no id was requested.
AVCaptureDevice *default_device() {
  for (AVCaptureDevice *device in video_devices()) {
    return device;
  }
  return nil;
}

/// Camera permission, requesting it once when the process has not decided yet.
///
/// A macOS CLI process inherits the permission of the terminal that launched it, so a denial
/// here means the invoking application lacks the camera entitlement. On iOS the request only
/// reaches the user at all when the host app's `Info.plist` declares
/// `NSCameraUsageDescription`; without that key the system terminates the process instead of
/// asking, so the shim cannot report that case as a failure.
int32_t ensure_video_access() {
  AVAuthorizationStatus status =
      [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
  if (status == AVAuthorizationStatusNotDetermined) {
    dispatch_semaphore_t semaphore = dispatch_semaphore_create(0);
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo
                             completionHandler:^(BOOL granted) {
                               (void)granted;
                               dispatch_semaphore_signal(semaphore);
                             }];
    // Bounded: a process without a proper bundle may never get an answer.
    dispatch_semaphore_wait(semaphore, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC));
    status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
  }
  if (status == AVAuthorizationStatusAuthorized) {
    return SCWC_MAC_OK;
  }

  NSString *reason = status == AVAuthorizationStatusDenied
                         ? @"denied"
                         : status == AVAuthorizationStatusRestricted ? @"restricted"
                                                                     : @"not granted";
#if TARGET_OS_IPHONE
  return fail(SCWC_MAC_ERR_PERMISSION,
              [NSString stringWithFormat:@"camera access is %@ for this app; the host app must "
                                         @"declare NSCameraUsageDescription in its Info.plist "
                                         @"and grant camera access in Settings > Privacy & "
                                         @"Security > Camera",
                                         reason]);
#else
  return fail(SCWC_MAC_ERR_PERMISSION,
              [NSString stringWithFormat:@"camera access is %@ for this process; grant it in "
                                         @"System Settings > Privacy & Security > Camera for "
                                         @"the application that launched it",
                                         reason]);
#endif
}

/// Converts an AVCC sample to Annex B, prepending parameter sets in front of a keyframe.
///
/// The 4-byte length prefix of every NALU is rewritten into a start code in place, so the
/// only copies are the sample itself and, for a keyframe, its parameter sets.
bool annex_b(CMBlockBufferRef block, CMFormatDescriptionRef description, bool keyframe,
             std::vector<uint8_t> &out) {
  size_t length = CMBlockBufferGetDataLength(block);
  if (length == 0) {
    return false;
  }
  std::vector<uint8_t> payload(length);
  if (CMBlockBufferCopyDataBytes(block, 0, length, payload.data()) != kCMBlockBufferNoErr) {
    return false;
  }

  static const uint8_t start_code[4] = {0, 0, 0, 1};
  size_t offset = 0;
  while (offset + 4 <= length) {
    uint32_t nalu_length = ((uint32_t)payload[offset] << 24) |
                           ((uint32_t)payload[offset + 1] << 16) |
                           ((uint32_t)payload[offset + 2] << 8) | (uint32_t)payload[offset + 3];
    offset += 4;
    if (nalu_length == 0 || offset + nalu_length > length) {
      return false;
    }
    std::memcpy(payload.data() + offset - 4, start_code, sizeof(start_code));
    offset += nalu_length;
  }
  if (offset != length) {
    return false;
  }

  out.clear();
  if (keyframe) {
    out.reserve(length + 128);
    CMVideoCodecType codec = CMFormatDescriptionGetMediaSubType(description);
    size_t count = 0;
    if (codec == kCMVideoCodecType_H264) {
      if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(description, 0, nullptr, nullptr,
                                                              &count, nullptr) != noErr) {
        return false;
      }
      for (size_t index = 0; index < count; index++) {
        const uint8_t *set = nullptr;
        size_t set_length = 0;
        if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(description, index, &set, &set_length,
                                                               nullptr, nullptr) != noErr) {
          continue;
        }
        out.insert(out.end(), start_code, start_code + sizeof(start_code));
        out.insert(out.end(), set, set + set_length);
      }
    } else if (codec == kCMVideoCodecType_HEVC) {
      if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(description, 0, nullptr, nullptr,
                                                              &count, nullptr) != noErr) {
        return false;
      }
      for (size_t index = 0; index < count; index++) {
        const uint8_t *set = nullptr;
        size_t set_length = 0;
        if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(description, index, &set, &set_length,
                                                                nullptr, nullptr) != noErr) {
          continue;
        }
        out.insert(out.end(), start_code, start_code + sizeof(start_code));
        out.insert(out.end(), set, set + set_length);
      }
    } else {
      return false;
    }
  }
  out.insert(out.end(), payload.begin(), payload.end());
  return true;
}

/// Whether the sample is an IDR, i.e. carries no `NotSync` attachment.
bool sample_is_keyframe(CMSampleBufferRef sample) {
  CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
  if (attachments == NULL || CFArrayGetCount(attachments) == 0) {
    return true;
  }
  CFDictionaryRef attachment = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
  return !CFDictionaryContainsKey(attachment, kCMSampleAttachmentKey_NotSync);
}

/// Encodes one captured frame as JPEG.
///
/// The buffer comes straight off the capture connection, so the still is as the sensor
/// produced it: nothing rotates it here (see `captureOutput:didOutputSampleBuffer:…`).
NSData *encode_jpeg(CVPixelBufferRef pixel_buffer) {
  @autoreleasepool {
    CVPixelBufferLockBaseAddress(pixel_buffer, kCVPixelBufferLock_ReadOnly);
    CIImage *image = [CIImage imageWithCVPixelBuffer:pixel_buffer];

    static CIContext *context = nil;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
      context = [CIContext contextWithOptions:nil];
    });

    CGImageRef cg_image = [context createCGImage:image fromRect:image.extent];
    CVPixelBufferUnlockBaseAddress(pixel_buffer, kCVPixelBufferLock_ReadOnly);
    if (cg_image == NULL) {
      return nil;
    }

    NSMutableData *data = [NSMutableData data];
    // "public.jpeg" is the type identifier kUTTypeJPEG names; the constant itself is
    // deprecated and would drag in MobileCoreServices.
    CGImageDestinationRef destination = CGImageDestinationCreateWithData(
        (__bridge CFMutableDataRef)data, CFSTR("public.jpeg"), 1, NULL);
    if (destination == NULL) {
      CGImageRelease(cg_image);
      return nil;
    }
    NSDictionary *properties = @{
      (__bridge NSString *)kCGImageDestinationLossyCompressionQuality : @0.9,
    };
    CGImageDestinationAddImage(destination, cg_image, (__bridge CFDictionaryRef)properties);
    bool ok = CGImageDestinationFinalize(destination);
    CFRelease(destination);
    CGImageRelease(cg_image);
    return ok ? data : nil;
  }
}

/// The CoreImage context preview frames are converted with.
///
/// One context for the process, like the one the photo path uses: creating one per frame would
/// cost more than the conversion.
CIContext *preview_context() {
  static CIContext *context = nil;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    context = [CIContext contextWithOptions:nil];
  });
  return context;
}

/// The colour space a preview frame is converted into.
CGColorSpaceRef preview_color_space() {
  static CGColorSpaceRef space = NULL;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
  });
  return space;
}

/// One enumerated resolution with the frame rates announced for it.
struct ResolutionRow {
  uint32_t width;
  uint32_t height;
  std::vector<uint32_t> fps;
};

/// VideoToolbox hands finished access units back through this C callback.
extern "C" void vt_output_callback(void *refcon, void *frame_refcon, OSStatus status,
                                   VTEncodeInfoFlags flags,
                                   CMSampleBufferRef sample_buffer) {
  (void)frame_refcon;
  (void)flags;
  ScwcMacCapture *capture = (__bridge ScwcMacCapture *)refcon;
  [capture handleEncodedSample:sample_buffer status:status];
}

}  // namespace

@implementation ScwcMacCapture

- (instancetype)init {
  self = [super init];
  if (self != nil) {
    _delegateQueue = dispatch_queue_create("scwc.capture.frames", DISPATCH_QUEUE_SERIAL);
    _session = nil;
    _output = nil;
    _device = nil;
    _format = nil;
    _encoder = NULL;
    _latestPixelBuffer = NULL;
    _frameCallback = NULL;
    _frameUser = NULL;
    _statusCallback = NULL;
    _statusUser = NULL;
    _previewCallback = NULL;
    _previewUser = NULL;
    _previewWanted = NULL;
    _codec = 0;
    _width = 0;
    _height = 0;
    _fps = 0;
    _bitrateKbps = 0;
    _shutDown = false;
    _failureReported = false;
    _runtimeErrorObserver = nil;
    _interruptionObserver = nil;
#if TARGET_OS_IPHONE
    _thermalObserver = nil;
    _interruptionEndedObserver = nil;
    // No state has been reported yet, so the first one this shim sees is a transition.
    _lastThermalState = -1;
#endif
  }
  return self;
}

- (void)dealloc {
  [self teardown];
}

- (int32_t)startWithConfig:(const scwc_mac_config *)cfg
              frameCallback:(scwc_mac_frame_cb)callback
                       user:(void *)user {
  if (cfg == NULL || callback == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"capture started without a configuration or a callback");
  }
  if (cfg->width == 0 || cfg->height == 0 || cfg->fps == 0 || cfg->bitrate_kbps == 0 ||
      cfg->fps > 1000) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"invalid capture parameters %ux%u@%u with %u kbps",
                                           cfg->width, cfg->height, cfg->fps, cfg->bitrate_kbps]);
  }

  _cameraId = from_c_string(cfg->camera_id);
  [self setCallback:callback user:user];

  int32_t code = [self selectDevice];
  if (code != SCWC_MAC_OK) {
    return code;
  }

  CaptureParameters parameters = {cfg->width, cfg->height, cfg->fps, cfg->codec,
                                  cfg->bitrate_kbps};
  AVCaptureDeviceFormat *format = nil;
  AVCaptureSession *session = nil;
  AVCaptureVideoDataOutput *output = nil;
  VTCompressionSessionRef encoder = NULL;
  code = [self buildPipelineWithParameters:parameters
                                   format:&format
                                  session:&session
                                   output:&output
                                  encoder:&encoder];
  if (code != SCWC_MAC_OK) {
    return code;
  }
  code = [self applyFormat:format fps:parameters.fps];
  if (code != SCWC_MAC_OK) {
    // Nothing has been published yet, so only the encoder built above has to go.
    [self disposeEncoder:encoder];
    return code;
  }

  [self publishParameters:parameters format:format session:session output:output encoder:encoder];
  [self subscribeNotifications];
  return [self startLiveSession];
}

- (int32_t)setWidth:(uint32_t)width
             height:(uint32_t)height
                fps:(uint32_t)fps
              codec:(int32_t)codec
        bitrateKbps:(uint32_t)bitrate {
  if (width == 0 || height == 0 || fps == 0 || bitrate == 0 || fps > 1000) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"invalid capture parameters %ux%u@%u with %u kbps",
                                           width, height, fps, bitrate]);
  }
  if (_shutDown) {
    return fail(SCWC_MAC_ERR_OS, @"the capture pipeline is closed");
  }
  if (width == _width && height == _height && fps == _fps && codec == _codec &&
      bitrate == _bitrateKbps) {
    return SCWC_MAC_OK;
  }

  CaptureParameters parameters = {width, height, fps, codec, bitrate};

  // The replacement is built and validated before the running pipeline is touched, so a
  // request the device or VideoToolbox refuses leaves the camera streaming on the parameters
  // it already has.
  AVCaptureDeviceFormat *format = nil;
  AVCaptureSession *session = nil;
  AVCaptureVideoDataOutput *output = nil;
  VTCompressionSessionRef encoder = NULL;
  int32_t code = [self buildPipelineWithParameters:parameters
                                            format:&format
                                           session:&session
                                            output:&output
                                           encoder:&encoder];
  if (code != SCWC_MAC_OK) {
    return code;
  }

  // The live pipeline, kept until the replacement runs: it is what a failure puts back.
  CaptureParameters live = {_width, _height, _fps, _codec, _bitrateKbps};
  AVCaptureDeviceFormat *liveFormat = _format;
  AVCaptureSession *liveSession = _session;
  AVCaptureVideoDataOutput *liveOutput = _output;
  VTCompressionSessionRef liveEncoder = _encoder;

  // Capture stops here: the device is handed to the replacement, and a frame still queued for
  // the old encoder would otherwise race the swap. The session keeps its input and output, so
  // it can start again as it is.
  [self unsubscribeNotifications];
  [self stopLiveSession];

  code = [self applyFormat:format fps:parameters.fps];
  if (code != SCWC_MAC_OK) {
    // The device kept the live format, and the live pipeline only has to start again.
    [self restorePipeline:live
                   format:liveFormat
                  session:liveSession
                   output:liveOutput
                  encoder:liveEncoder];
    [self disposeEncoder:encoder];
    return code;
  }

  [self publishParameters:parameters format:format session:session output:output encoder:encoder];
  [self subscribeNotifications];
  code = [self startLiveSession];
  if (code != SCWC_MAC_OK) {
    // The replacement never ran, so the pipeline that was streaming goes back in its place,
    // on the parameters it was using.
    [self restorePipeline:live
                   format:liveFormat
                  session:liveSession
                   output:liveOutput
                  encoder:liveEncoder];
    [self disposeEncoder:encoder];
    return code;
  }

  [self disposeEncoder:liveEncoder];
  return SCWC_MAC_OK;
}

- (int32_t)takePhotoWithCallback:(scwc_mac_jpeg_cb)callback user:(void *)user {
  if (callback == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"photo requested without a callback");
  }

  // One photo at a time: the encode below reads one retained frame and runs to completion
  // before this call returns.
  std::unique_lock<std::mutex> in_flight(_photoInFlight, std::try_to_lock);
  if (!in_flight.owns_lock()) {
    return fail(SCWC_MAC_ERR_BUSY, @"a photo capture is already in flight");
  }

  CVPixelBufferRef frame = NULL;
  {
    std::lock_guard<std::mutex> guard(_photoLock);
    if (_latestPixelBuffer != NULL) {
      CFRetain(_latestPixelBuffer);
      frame = _latestPixelBuffer;
    }
  }
  if (frame == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"no frame has been captured yet, so there is no photo");
  }

  NSData *jpeg = encode_jpeg(frame);
  CFRelease(frame);
  if (jpeg == nil) {
    return fail(SCWC_MAC_ERR_OS, @"JPEG encoding of the captured frame failed");
  }

  callback(user, (const uint8_t *)jpeg.bytes, (size_t)jpeg.length);
  return SCWC_MAC_OK;
}

- (void)shutdown {
  [self teardown];
}

/// Publishes the frame callback, re-enabling dispatch after a teardown.
- (void)setCallback:(scwc_mac_frame_cb)callback user:(void *)user {
  std::lock_guard<std::mutex> guard(_callbackLock);
  _frameCallback = callback;
  _frameUser = user;
  _shutDown = false;
}

/// Publishes the device status callback; `NULL` stops the reporting.
///
/// This outlives a reconfigure: `teardown` drops the frame callback but not this one, so a
/// pipeline rebuilt by `setWidth:…` keeps reporting status.
- (void)setStatusCallback:(scwc_mac_status_cb)callback user:(void *)user {
  std::lock_guard<std::mutex> guard(_callbackLock);
  _statusCallback = callback;
  _statusUser = user;
}

/// Publishes the preview callback and the gate it is consulted through; `NULL` stops it.
///
/// Like the status callback, this outlives a reconfigure: a `switch_camera` changes what the
/// camera captures, not whether its picture is shown.
- (void)setPreviewCallback:(scwc_mac_preview_cb)callback
                      user:(void *)user
                    wanted:(scwc_mac_preview_wanted_cb)wanted {
  std::lock_guard<std::mutex> guard(_previewLock);
  _previewCallback = callback;
  _previewUser = user;
  _previewWanted = wanted;
  if (callback == NULL) {
    // Nothing will ask for the pixels again, so the buffer can go.
    _previewPixels.clear();
    _previewPixels.shrink_to_fit();
  }
}

/// Converts one captured frame to tightly packed BGRA and hands it to Rust.
///
/// Runs on the capture queue, for every frame, and only while Rust says the picture is wanted:
/// the conversion reads the whole frame, so a preview nobody looks at must not pay for it. The
/// frame is handed over at the camera's own size — scaling it to the size the interface shows
/// it in is the Rust side's job — as one tightly packed buffer, so Rust needs no knowledge of
/// CoreVideo's row padding.
- (void)publishPreview:(CVPixelBufferRef)pixel_buffer {
  std::lock_guard<std::mutex> guard(_previewLock);
  if (_previewCallback == NULL || _previewWanted == NULL || _previewWanted(_previewUser) == 0) {
    return;
  }

  const size_t width = CVPixelBufferGetWidth(pixel_buffer);
  const size_t height = CVPixelBufferGetHeight(pixel_buffer);
  if (width == 0 || height == 0) {
    return;
  }

  const size_t row_bytes = width * 4;
  _previewPixels.resize(row_bytes * height);
  @autoreleasepool {
    CIImage *image = [CIImage imageWithCVPixelBuffer:pixel_buffer];
    [preview_context() render:image
                     toBitmap:_previewPixels.data()
                     rowBytes:(ptrdiff_t)row_bytes
                       bounds:image.extent
                       format:kCIFormatBGRA8
                   colorSpace:preview_color_space()];
  }

  _previewCallback(_previewUser, _previewPixels.data(), _previewPixels.size(), (uint32_t)width,
                   (uint32_t)height);
}

/// Reports one hard failure of the pipeline through the status callback, at most once.
///
/// A pipeline that fails keeps failing — an encoder whose session died reports a status on
/// every frame — so only the first failure is reported: Rust learns that capture broke, and
/// the platform log names it once instead of once per frame. The report is reset when a new
/// pipeline takes over, which gets its own first failure.
- (void)reportFailure:(NSString *)message {
  std::lock_guard<std::mutex> guard(_callbackLock);
  [self reportFailureLocked:message];
}

/// `reportFailure:` for the paths that already hold `_callbackLock`.
- (void)reportFailureLocked:(NSString *)message {
  if (_failureReported) {
    return;
  }
  _failureReported = true;
  NSLog(@"[scwc-capture] %@", message);
  if (_shutDown || _statusCallback == NULL) {
    return;
  }
  _statusCallback(_statusUser, SCWC_MAC_STATUS_FAILURE);
}

/// Subscribes to the notifications that report a failed session, and on iOS the thermal
/// pressure and the interruptions a phone call or a background app causes.
///
/// A session that dies mid-stream says so through a runtime error on both platforms — on
/// macOS that notification is the only word the shim ever gets about it — so it is the one
/// subscription both platforms share. macOS adds the interruption another application taking
/// the camera causes; the system hands the device back on its own once that application is
/// done, so nothing there restarts anything.
- (void)subscribeNotifications {
  __weak ScwcMacCapture *weakSelf = self;
  // The observers are registered against the session, and the restart below only applies to
  // that same session: a reconfigure builds a new one.
  AVCaptureSession *session = _session;

  _runtimeErrorObserver = [[NSNotificationCenter defaultCenter]
      addObserverForName:AVCaptureSessionRuntimeErrorNotification
                  object:session
                   queue:nil
              usingBlock:^(NSNotification *notification) {
                ScwcMacCapture *capture = weakSelf;
                if (capture == nil) {
                  return;
                }
                NSError *error = notification.userInfo[AVCaptureSessionErrorKey];
                NSString *reason = error != nil ? error.localizedDescription : @"unknown reason";
                [capture reportFailure:[NSString stringWithFormat:@"the capture session failed "
                                                                   @"at runtime: %@",
                                                                  reason]];
              }];

#if TARGET_OS_IPHONE
  if (@available(iOS 11.0, *)) {
    _thermalObserver = [[NSNotificationCenter defaultCenter]
        addObserverForName:NSProcessInfoThermalStateDidChangeNotification
                    object:nil
                     queue:nil
                usingBlock:^(NSNotification *notification) {
                  (void)notification;
                  ScwcMacCapture *capture = weakSelf;
                  if (capture != nil) {
                    // Only logging and a status callback: safe on the posting thread.
                    [capture reportThermalState:(int32_t)[NSProcessInfo processInfo].thermalState];
                  }
                }];
    // The state at subscription is the first transition this shim has to report.
    [self reportThermalState:(int32_t)[NSProcessInfo processInfo].thermalState];
  }

  // The interruption handlers only log, restart and take the shim's locks, but they arrive on
  // the thread that posted the notification, so the restart is handed to the delegate queue:
  // AVFoundation calls then stay off that foreign thread, and the restart cannot interleave
  // with `teardown`, which stops and drops the session under `_sessionLock` before it drains
  // that queue.
  _interruptionObserver = [[NSNotificationCenter defaultCenter]
      addObserverForName:AVCaptureSessionWasInterruptedNotification
                  object:session
                   queue:nil
              usingBlock:^(NSNotification *notification) {
                NSNumber *reason = notification.userInfo[AVCaptureSessionInterruptionReasonKey];
                NSLog(@"[scwc-capture] the capture session was interrupted (%@); capture stays "
                       "paused until the interruption ends",
                      reason);
              }];

  _interruptionEndedObserver = [[NSNotificationCenter defaultCenter]
      addObserverForName:AVCaptureSessionInterruptionEndedNotification
                  object:session
                   queue:nil
              usingBlock:^(NSNotification *notification) {
                (void)notification;
                ScwcMacCapture *capture = weakSelf;
                if (capture == nil) {
                  return;
                }
                dispatch_async(capture->_delegateQueue, ^{
                  ScwcMacCapture *capture = weakSelf;
                  if (capture != nil) {
                    [capture resumeAfterInterruption:session];
                  }
                });
              }];
#else
  // Nothing to restart here: the notification carries no interruption reason on macOS, and
  // the system resumes what it interrupted.
  _interruptionObserver = [[NSNotificationCenter defaultCenter]
      addObserverForName:AVCaptureSessionWasInterruptedNotification
                  object:session
                   queue:nil
              usingBlock:^(NSNotification *notification) {
                (void)notification;
                ScwcMacCapture *capture = weakSelf;
                if (capture != nil) {
                  [capture reportFailure:@"the capture session was interrupted; another "
                                          @"application may have taken the camera"];
                }
              }];
#endif
}

/// Cancels the subscriptions; afterwards no notification block reaches this object again.
///
/// A block that is already running is left to finish, which is why the interrupt handlers
/// check the lock-protected state before they act.
- (void)unsubscribeNotifications {
  NSNotificationCenter *center = [NSNotificationCenter defaultCenter];
  if (_runtimeErrorObserver != nil) {
    [center removeObserver:_runtimeErrorObserver];
    _runtimeErrorObserver = nil;
  }
  if (_interruptionObserver != nil) {
    [center removeObserver:_interruptionObserver];
    _interruptionObserver = nil;
  }
#if TARGET_OS_IPHONE
  if (_thermalObserver != nil) {
    [center removeObserver:_thermalObserver];
    _thermalObserver = nil;
  }
  if (_interruptionEndedObserver != nil) {
    [center removeObserver:_interruptionEndedObserver];
    _interruptionEndedObserver = nil;
  }
#endif
}

/// Makes `format` the device's active format, at `fps`.
///
/// The device is one object, shared by every session of the process: the change reaches the
/// session that is running through it, which is why this is only called once the pipeline
/// being replaced is done with the device.
- (int32_t)applyFormat:(AVCaptureDeviceFormat *)format fps:(uint32_t)fps {
  if (format == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, @"no capture format is available for the device");
  }

  NSError *error = nil;
  if (![_device lockForConfiguration:&error]) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"cannot configure %@: %@",
                                                            _device.localizedName,
                                                            error.localizedDescription]);
  }
  // AVFoundation raises rather than returns an error for a format or duration it rejects.
  @try {
    [_device setActiveFormat:format];
    CMTime duration = frame_duration(format, fps);
    [_device setActiveVideoMinFrameDuration:duration];
    [_device setActiveVideoMaxFrameDuration:duration];
  } @catch (NSException *exception) {
    [_device unlockForConfiguration];
    CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(format.formatDescription);
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"%@ rejected %ux%u at %u fps: %@",
                                           _device.localizedName, (uint32_t)dimensions.width,
                                           (uint32_t)dimensions.height, fps, exception.reason]);
  }
  [_device unlockForConfiguration];
  return SCWC_MAC_OK;
}

/// Makes the pipeline built for `parameters` the live one.
- (void)publishParameters:(const CaptureParameters &)parameters
                   format:(AVCaptureDeviceFormat *)format
                  session:(AVCaptureSession *)session
                   output:(AVCaptureVideoDataOutput *)output
                  encoder:(VTCompressionSessionRef)encoder {
  _width = parameters.width;
  _height = parameters.height;
  _fps = parameters.fps;
  _codec = parameters.codec;
  _bitrateKbps = parameters.bitrate_kbps;
  _format = format;
  {
    // Held while the live session changes hands: it is the lock the iOS interruption handler
    // takes, so a session that is no longer the live one is never restarted by a late
    // interruption notification.
    std::lock_guard<std::mutex> guard(_sessionLock);
    _session = session;
    _output = output;
  }
  _encoder = encoder;
  // The still `take_photo` would hand out belongs to the pipeline that just went away, so the
  // next frame of this one is the first still it can offer.
  {
    std::lock_guard<std::mutex> guard(_photoLock);
    if (_latestPixelBuffer != NULL) {
      CFRelease(_latestPixelBuffer);
      _latestPixelBuffer = NULL;
    }
  }
  // The pipeline that starts here gets its own first failure to report; the one before it has
  // used up the report the operator already has.
  std::lock_guard<std::mutex> guard(_callbackLock);
  _failureReported = false;
}

/// Puts a pipeline that was running back in place, with the parameters it had.
///
/// Used when a reconfiguration fails after it took the running pipeline out of service: the
/// session kept its input and its output, so it starts again on the format and parameters the
/// operator chose before the request.
- (void)restorePipeline:(const CaptureParameters &)parameters
                 format:(AVCaptureDeviceFormat *)format
                session:(AVCaptureSession *)session
                 output:(AVCaptureVideoDataOutput *)output
                encoder:(VTCompressionSessionRef)encoder {
  [self unsubscribeNotifications];
  [self publishParameters:parameters format:format session:session output:output encoder:encoder];
  (void)[self applyFormat:format fps:parameters.fps];
  [self subscribeNotifications];
  // Best effort: the failure being returned is the one that has to reach the caller.
  (void)[self startLiveSession];
}

/// Starts the live session.
- (int32_t)startLiveSession {
  AVCaptureSession *session = _session;
  if (session == nil) {
    return fail(SCWC_MAC_ERR_OS, @"the capture session is gone");
  }
  @try {
    [session startRunning];
  } @catch (NSException *exception) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"capture session failed to start: %@",
                                                            exception.reason]);
  }
  if (![session isRunning]) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"capture session did not start for %@",
                                                            _device.localizedName]);
  }
  return SCWC_MAC_OK;
}

/// Takes the live session out of service and drains the delegate queue.
///
/// Used before a replacement pipeline takes over: a frame already queued for the delegate or
/// inside the encoder finishes before this returns, so nothing that follows races the old
/// pipeline. The session itself, its input and its output are left alone — that is what lets
/// a failed rebuild start it again — but it is no longer the live session, so an interruption
/// ending on the delegate queue cannot restart what is being replaced.
- (void)stopLiveSession {
  AVCaptureSession *session = nil;
  {
    // Shared with the iOS interruption handler, which restarts the session from the delegate
    // queue.
    std::lock_guard<std::mutex> guard(_sessionLock);
    session = _session;
    _session = nil;
    _output = nil;
  }
  if (session != nil && [session isRunning]) {
    [session stopRunning];
  }
  // Drain the delegate queue: a `captureOutput:` already enqueued would run otherwise.
  if (_delegateQueue != NULL) {
    dispatch_sync(_delegateQueue, ^{
    });
  }
}

/// Invalidates an encoder and releases it once no callback can still be inside it.
- (void)disposeEncoder:(VTCompressionSessionRef)encoder {
  if (encoder == NULL) {
    return;
  }
  // The frames it still holds are finished and delivered before it is torn down: an
  // invalidation with frames pending returns each of them as a failure, and a pipeline that
  // is only being replaced has not failed. Release afterwards, under the callback lock, once
  // no callback can still be inside the encoder — VideoToolbox then has no thread left to run
  // this shim's callback on.
  VTCompressionSessionCompleteFrames(encoder, kCMTimeInvalid);
  VTCompressionSessionInvalidate(encoder);
  std::lock_guard<std::mutex> guard(_callbackLock);
  CFRelease(encoder);
}

#if TARGET_OS_IPHONE
/// Logs one thermal transition and forwards it to Rust.
///
/// The protocol's answer to heat is an encoder bitrate reduction — the resolution and the
/// frame rate are the operator's to change — and that policy lives in the core; the shim
/// only reports what the system says.
- (void)reportThermalState:(int32_t)state {
  scwc_mac_status_cb callback = NULL;
  void *user = NULL;
  {
    std::lock_guard<std::mutex> guard(_callbackLock);
    if (_shutDown || state == _lastThermalState) {
      return;
    }
    _lastThermalState = state;
    callback = _statusCallback;
    user = _statusUser;
  }

  NSLog(@"[scwc-capture] the device thermal state is now %d (0 nominal, 3 critical)", (int)state);
  if (callback != NULL) {
    callback(user, state);
  }
}

/// Restarts a session that an interruption stopped.
///
/// AVFoundation restarts a session after an interruption when it deems that appropriate; a
/// phone call keeps it stopped until it ends, which is what this is for. Only the session the
/// interruption belonged to is restarted — a reconfigure may have replaced it in the
/// meantime — and one that is already running, or whose pipeline is gone, is left alone.
- (void)resumeAfterInterruption:(AVCaptureSession *)session {
  {
    std::lock_guard<std::mutex> guard(_callbackLock);
    if (_shutDown) {
      return;
    }
  }

  // Held while the session starts: it is the lock `teardown` takes to stop and drop the
  // session, so the two cannot interleave.
  std::lock_guard<std::mutex> guard(_sessionLock);
  if (_session != session || [session isRunning]) {
    return;
  }
  NSLog(@"[scwc-capture] the interruption ended, resuming capture");
  @try {
    [session startRunning];
  } @catch (NSException *exception) {
    NSLog(@"[scwc-capture] the capture session did not resume: %@", exception.reason);
  }
}
#endif

/// Stops the session and the encoder; afterwards no callback can reach Rust.
- (void)teardown {
  // First, so that no further notification is posted into this object.
  [self unsubscribeNotifications];

  {
    std::lock_guard<std::mutex> guard(_callbackLock);
    _shutDown = true;
    _frameCallback = NULL;
    _frameUser = NULL;
  }

  if (_output != nil) {
    [_output setSampleBufferDelegate:nil queue:nil];
  }
  {
    // Shared with the iOS interruption handler, which restarts the session from the
    // delegate queue.
    std::lock_guard<std::mutex> guard(_sessionLock);
    if (_session != nil) {
      if ([_session isRunning]) {
        [_session stopRunning];
      }
      for (AVCaptureInput *input in [_session inputs]) {
        [_session removeInput:input];
      }
      for (AVCaptureOutput *output in [_session outputs]) {
        [_session removeOutput:output];
      }
      _session = nil;
      _output = nil;
      _format = nil;
    }
  }
  // Drain the delegate queue: a `captureOutput:` — or an interruption handler — already
  // enqueued would run otherwise.
  if (_delegateQueue != NULL) {
    dispatch_sync(_delegateQueue, ^{
    });
  }

  if (_encoder != NULL) {
    // Invalidate returns pending frames with an error status; release afterwards, once no
    // callback can still be inside the encoder, so VideoToolbox has no thread left to run
    // this shim's callback on.
    VTCompressionSessionInvalidate(_encoder);
    {
      std::lock_guard<std::mutex> guard(_callbackLock);
      if (_encoder != NULL) {
        CFRelease(_encoder);
        _encoder = NULL;
      }
    }
  }
  {
    std::lock_guard<std::mutex> guard(_photoLock);
    if (_latestPixelBuffer != NULL) {
      CFRelease(_latestPixelBuffer);
      _latestPixelBuffer = NULL;
    }
  }
}

/// Resolves the requested device, or the system default one.
- (int32_t)selectDevice {
  AVCaptureDevice *device = nil;
  if (!_cameraId.empty()) {
    device = [AVCaptureDevice deviceWithUniqueID:[NSString stringWithUTF8String:_cameraId.c_str()]];
  } else {
    device = default_device();
  }
  if (device == nil) {
    return fail(SCWC_MAC_ERR_NO_CAMERA,
                _cameraId.empty()
                    ? @"no video capture device found"
                    : [NSString stringWithFormat:@"no capture device with id %s", _cameraId.c_str()]);
  }
  _device = device;
  return SCWC_MAC_OK;
}

/// Builds a capture pipeline for `parameters`, without publishing or starting it.
///
/// The pipeline that is running is left alone — including the device's active format, which
/// is `applyFormat:`'s to set — so a build the device or VideoToolbox refuses cannot disturb
/// the frames it is delivering. The caller owns the encoder it gets back.
- (int32_t)buildPipelineWithParameters:(const CaptureParameters &)parameters
                                format:(AVCaptureDeviceFormat *__strong *)outFormat
                               session:(AVCaptureSession *__strong *)outSession
                                output:(AVCaptureVideoDataOutput *__strong *)outOutput
                               encoder:(VTCompressionSessionRef *)outEncoder {
  if (_device == nil) {
    return fail(SCWC_MAC_ERR_NO_CAMERA, @"the capture device is gone");
  }

  AVCaptureDeviceFormat *format =
      pick_format(_device, parameters.width, parameters.height, parameters.fps);
  if (format == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"%@ cannot capture %ux%u at %u fps",
                                           _device.localizedName, parameters.width,
                                           parameters.height, parameters.fps]);
  }

  AVCaptureSession *session = [[AVCaptureSession alloc] init];

#if TARGET_OS_IPHONE
  // Without this preset, adding the input lets the session configure the device to suit its
  // own default preset, which would throw away the format and frame rate chosen above. The
  // input-priority preset says the input dictates the format instead; it does not exist on
  // macOS, where the session adopts the active format on its own, so macOS is left alone.
  if ([session canSetSessionPreset:AVCaptureSessionPresetInputPriority]) {
    [session setSessionPreset:AVCaptureSessionPresetInputPriority];
  } else {
    NSLog(@"[scwc-capture] the capture session refused the input-priority preset; it may "
           "override the selected format");
  }
#endif

  NSError *error = nil;
  AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:_device error:&error];
  if (input == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, [NSString stringWithFormat:@"cannot capture from %@: %@",
                                                                    _device.localizedName,
                                                                    error.localizedDescription]);
  }
  if (![session canAddInput:input]) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, @"the capture session refused the device input");
  }
  [session addInput:input];

  AVCaptureVideoDataOutput *output = [[AVCaptureVideoDataOutput alloc] init];
  // Late frames are dropped by the session rather than queued; the encoder is the throttle.
  [output setAlwaysDiscardsLateVideoFrames:YES];
  [output setVideoSettings:@{
    (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange),
  }];
  [output setSampleBufferDelegate:self queue:_delegateQueue];
  if (![session canAddOutput:output]) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, @"the capture session refused the video output");
  }
  [session addOutput:output];

  VTCompressionSessionRef encoder = NULL;
  int32_t code = [self createEncoderWithParameters:parameters encoder:&encoder];
  if (code != SCWC_MAC_OK) {
    return code;
  }

  *outFormat = format;
  *outSession = session;
  *outOutput = output;
  *outEncoder = encoder;
  return SCWC_MAC_OK;
}

/// Creates the VideoToolbox encoder for `parameters`; the caller owns the encoder.
- (int32_t)createEncoderWithParameters:(const CaptureParameters &)parameters
                               encoder:(VTCompressionSessionRef *)outEncoder {
  CMVideoCodecType codec = parameters.codec == 1 ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264;
  VTCompressionSessionRef encoder = NULL;
  OSStatus status = VTCompressionSessionCreate(kCFAllocatorDefault, (int32_t)parameters.width,
                                                (int32_t)parameters.height, codec, NULL, NULL,
                                                kCFAllocatorDefault, vt_output_callback,
                                                (__bridge void *)self, &encoder);
  if (status != noErr || encoder == NULL) {
    // Nothing else can reach a session the call reported a failure for, so it goes here.
    if (encoder != NULL) {
      CFRelease(encoder);
    }
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"VideoToolbox refused %ux%u (%d)",
                                                            parameters.width, parameters.height,
                                                            (int)status]);
  }

  set_property(encoder, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
  // A GOP of two seconds bounds how long a late joiner waits for a keyframe.
  set_property(encoder, kVTCompressionPropertyKey_MaxKeyFrameInterval,
               (__bridge CFTypeRef)@(2 * parameters.fps));
  set_property(encoder, kVTCompressionPropertyKey_AverageBitRate,
               (__bridge CFTypeRef)@(parameters.bitrate_kbps * 1000u));
  set_property(encoder, kVTCompressionPropertyKey_ExpectedFrameRate,
               (__bridge CFTypeRef)@(parameters.fps));
  // No B-frames: reordering would buy little bandwidth and cost latency.
  set_property(encoder, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
  set_property(encoder, kVTCompressionPropertyKey_ProfileLevel,
               parameters.codec == 1 ? kVTProfileLevel_HEVC_Main_AutoLevel
                                     : kVTProfileLevel_H264_High_AutoLevel);

  status = VTCompressionSessionPrepareToEncodeFrames(encoder);
  if (status != noErr) {
    // The caller never sees this one, so it is invalidated and released here.
    VTCompressionSessionInvalidate(encoder);
    CFRelease(encoder);
    return fail(SCWC_MAC_ERR_OS,
                [NSString stringWithFormat:@"VideoToolbox could not prepare the encoder (%d)",
                                           (int)status]);
  }

  *outEncoder = encoder;
  return SCWC_MAC_OK;
}

/// Keeps the newest frame for `take_photo` and hands it to the encoder.
///
/// The frame is passed on as the connection delivers it: this shim never sets a rotation on
/// the connection, so nothing here turns the sensor's picture. Both the encoder and
/// `take_photo` therefore see the sensor-native orientation, and rotating it to the
/// interface orientation is the app layer's job — the protocol has no rotation field to
/// describe it in. (On iOS 17+ that app layer is `AVCaptureDeviceRotationCoordinator`, which
/// is also what knows the connection's default rotation, not always zero on the front camera
/// of newer iPads.)
- (void)captureOutput:(AVCaptureOutput *)output
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
           fromConnection:(AVCaptureConnection *)connection {
  (void)output;
  (void)connection;

  CVPixelBufferRef pixel_buffer = CMSampleBufferGetImageBuffer(sampleBuffer);
  if (pixel_buffer == NULL) {
    return;
  }
  {
    std::lock_guard<std::mutex> guard(_photoLock);
    CFRetain(pixel_buffer);
    if (_latestPixelBuffer != NULL) {
      CFRelease(_latestPixelBuffer);
    }
    _latestPixelBuffer = pixel_buffer;
  }

  // The preview is fed from the same frame the encoder gets, so the picture the operator sees
  // is the one being recorded. It does nothing at all while no preview is wanted.
  [self publishPreview:pixel_buffer];

  VTCompressionSessionRef encoder = _encoder;
  if (encoder == NULL) {
    return;
  }
  CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
  OSStatus status = VTCompressionSessionEncodeFrame(encoder, pixel_buffer, pts, kCMTimeInvalid,
                                                     NULL, NULL, NULL);
  if (status != noErr) {
    // A frame the encoder refuses is the first sign of a pipeline that has stopped encoding.
    [self reportFailure:[NSString stringWithFormat:@"encoding a frame failed: %d", (int)status]];
  }
}

- (void)handleEncodedSample:(CMSampleBufferRef)sample status:(OSStatus)status {
  // The lock is held for the whole body: teardown waits on it before it invalidates the
  // session, so no callback can be running once that returns.
  std::lock_guard<std::mutex> guard(_callbackLock);
  if (_shutDown || _frameCallback == NULL) {
    return;
  }
  if (status != noErr || sample == NULL) {
    // The lock is already held: this is the fail path of a frame the encoder gave up on.
    [self reportFailureLocked:[NSString stringWithFormat:@"the encoder reported status %d",
                                                         (int)status]];
    return;
  }
  if (!CMSampleBufferDataIsReady(sample)) {
    return;
  }

  CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
  CMFormatDescriptionRef description = CMSampleBufferGetFormatDescription(sample);
  if (block == NULL || description == NULL) {
    return;
  }

  bool keyframe = sample_is_keyframe(sample);
  std::vector<uint8_t> access_unit;
  if (!annex_b(block, description, keyframe, access_unit)) {
    NSLog(@"[scwc-capture] a sample was not in the expected AVCC layout");
    return;
  }

  _frameCallback(_frameUser, access_unit.data(), access_unit.size(), keyframe ? 1 : 0);
}

@end

extern "C" int32_t scwc_mac_enumerate(scwc_mac_camera_begin_cb begin_cb,
                                     scwc_mac_camera_format_cb format_cb, void *user) {
  if (begin_cb == NULL || format_cb == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"enumeration without callbacks");
  }

  @autoreleasepool {
    int32_t access = ensure_video_access();
    if (access != SCWC_MAC_OK) {
      return access;
    }

    NSArray<AVCaptureDevice *> *devices = video_devices();
    if (devices.count == 0) {
      return fail(SCWC_MAC_ERR_NO_CAMERA, @"no video capture device found");
    }

    for (AVCaptureDevice *device in devices) {
      NSString *name = device_name(device);
      begin_cb(user, device.uniqueID.UTF8String, name.UTF8String);

      // One row per resolution the device announces, with its announced frame rates.
      std::vector<ResolutionRow> rows;
      for (AVCaptureDeviceFormat *format in device.formats) {
        CMFormatDescriptionRef description = format.formatDescription;
        if (CMFormatDescriptionGetMediaType(description) != kCMMediaType_Video) {
          continue;
        }
        CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(description);
        if (dimensions.width <= 0 || dimensions.height <= 0) {
          continue;
        }

        ResolutionRow *row = nullptr;
        for (ResolutionRow &candidate : rows) {
          if (candidate.width == (uint32_t)dimensions.width &&
              candidate.height == (uint32_t)dimensions.height) {
            row = &candidate;
            break;
          }
        }
        if (row == nullptr) {
          rows.push_back(ResolutionRow{(uint32_t)dimensions.width, (uint32_t)dimensions.height, {}});
          row = &rows.back();
        }

        for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
          double fps = range_max_fps(range);
          if (!std::isfinite(fps) || fps < 1.0) {
            continue;
          }
          uint32_t rounded = (uint32_t)std::floor(fps);
          if (rounded == 0) {
            continue;
          }
          if (std::find(row->fps.begin(), row->fps.end(), rounded) == row->fps.end()) {
            row->fps.push_back(rounded);
          }
        }
      }

      std::sort(rows.begin(), rows.end(), [](const ResolutionRow &a, const ResolutionRow &b) {
        uint64_t area_a = (uint64_t)a.width * a.height;
        uint64_t area_b = (uint64_t)b.width * b.height;
        if (area_a != area_b) {
          return area_a > area_b;
        }
        return a.width > b.width;
      });

      for (ResolutionRow &row : rows) {
        std::sort(row.fps.begin(), row.fps.end(), std::greater<uint32_t>());
        std::string name = std::to_string(row.width) + "x" + std::to_string(row.height);
        for (uint32_t fps : row.fps) {
          format_cb(user, name.c_str(), fps);
        }
      }
    }
    return SCWC_MAC_OK;
  }
}

extern "C" int32_t scwc_mac_open(const scwc_mac_config *cfg, scwc_mac_frame_cb frame_cb, void *user,
                                 void **out_handle) {
  if (out_handle == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"capture started without an output handle");
  }
  *out_handle = NULL;
  if (cfg == NULL || frame_cb == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"capture started without a configuration or a callback");
  }

  @autoreleasepool {
    int32_t access = ensure_video_access();
    if (access != SCWC_MAC_OK) {
      return access;
    }

    ScwcMacCapture *capture = [[ScwcMacCapture alloc] init];
    int32_t code = [capture startWithConfig:cfg frameCallback:frame_cb user:user];
    if (code != SCWC_MAC_OK) {
      return code;
    }
    *out_handle = (__bridge_retained void *)capture;
    return SCWC_MAC_OK;
  }
}

extern "C" int32_t scwc_mac_set_status_cb(void *handle, scwc_mac_status_cb cb, void *user) {
  if (handle == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"status callback set without a handle");
  }
  ScwcMacCapture *capture = (__bridge ScwcMacCapture *)handle;
  [capture setStatusCallback:cb user:user];
  return SCWC_MAC_OK;
}

extern "C" int32_t scwc_mac_set_preview_cb(void *handle, scwc_mac_preview_cb cb, void *user,
                                           scwc_mac_preview_wanted_cb wanted) {
  if (handle == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"preview callback set without a handle");
  }
  if (cb != NULL && wanted == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"preview callback set without a gate");
  }
  ScwcMacCapture *capture = (__bridge ScwcMacCapture *)handle;
  [capture setPreviewCallback:cb user:user wanted:wanted];
  return SCWC_MAC_OK;
}

extern "C" int32_t scwc_mac_set_params(void *handle, uint32_t width, uint32_t height, uint32_t fps,
                                       int32_t codec, uint32_t bitrate_kbps) {
  if (handle == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"reconfiguration without a handle");
  }
  @autoreleasepool {
    ScwcMacCapture *capture = (__bridge ScwcMacCapture *)handle;
    return [capture setWidth:width height:height fps:fps codec:codec bitrateKbps:bitrate_kbps];
  }
}

extern "C" int32_t scwc_mac_take_photo(void *handle, scwc_mac_jpeg_cb jpeg_cb, void *user) {
  if (handle == NULL) {
    return fail(SCWC_MAC_ERR_OS, @"photo requested without a handle");
  }
  ScwcMacCapture *capture = (__bridge ScwcMacCapture *)handle;
  return [capture takePhotoWithCallback:jpeg_cb user:user];
}

extern "C" void scwc_mac_close(void *handle) {
  if (handle == NULL) {
    return;
  }
  @autoreleasepool {
    ScwcMacCapture *capture = (__bridge_transfer ScwcMacCapture *)handle;
    // The status and preview callbacks outlive a reconfigure, so they are dropped here
    // explicitly: after this returns, no callback of any kind can reach Rust.
    [capture setStatusCallback:NULL user:NULL];
    [capture setPreviewCallback:NULL user:NULL wanted:NULL];
    [capture shutdown];
  }
}

extern "C" int32_t scwc_mac_last_error(char *buf, size_t buf_len) {
  if (buf != NULL && buf_len > 0) {
    size_t length = std::min(buf_len - 1, gLastMessage.size());
    std::memcpy(buf, gLastMessage.data(), length);
    buf[length] = '\0';
  }
  return gLastCode;
}
