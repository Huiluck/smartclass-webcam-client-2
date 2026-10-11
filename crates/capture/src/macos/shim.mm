//
// Objective-C++ shim: AVFoundation capture and VideoToolbox encoding for `scwc-capture`.
//
// The shim owns every platform object behind an opaque handle. Rust never sees an
// Objective-C type: it calls the entry points at the bottom of this file and receives
// Annex B access units and JPEG stills through the callbacks it registers.
//
// Layout note: the class is declared first so the helpers below can message it, and the
// helpers come before the implementation that uses them.
//

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
  scwc_mac_frame_cb _frameCallback;
  void *_frameUser;
  int32_t _codec;
  uint32_t _width;
  uint32_t _height;
  uint32_t _fps;
  uint32_t _bitrateKbps;
  std::string _cameraId;
  bool _shutDown;
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
/// Flattens one encoded sample into an Annex B access unit and hands it to Rust.
- (void)handleEncodedSample:(CMSampleBufferRef)sample status:(OSStatus)status;
@end

namespace {

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

  AVCaptureDeviceDiscoverySession *session = [AVCaptureDeviceDiscoverySession
      discoverySessionWithDeviceTypes:types
                            mediaType:AVMediaTypeVideo
                             position:AVCaptureDevicePositionUnspecified];
  return session.devices;
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
/// A CLI process inherits the permission of the terminal that launched it, so a denial here
/// means the invoking application lacks the camera entitlement.
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
  return fail(SCWC_MAC_ERR_PERMISSION,
              [NSString stringWithFormat:@"camera access is %@ for this process; grant it in "
                                         @"System Settings > Privacy & Security > Camera for "
                                         @"the application that launched it",
                                         reason]);
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
    _codec = 0;
    _width = 0;
    _height = 0;
    _fps = 0;
    _bitrateKbps = 0;
    _shutDown = false;
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
  _width = cfg->width;
  _height = cfg->height;
  _fps = cfg->fps;
  _codec = cfg->codec;
  _bitrateKbps = cfg->bitrate_kbps;
  [self setCallback:callback user:user];

  int32_t code = [self selectDevice];
  if (code != SCWC_MAC_OK) {
    return code;
  }
  code = [self buildPipeline];
  if (code != SCWC_MAC_OK) {
    [self teardown];
    return code;
  }
  return SCWC_MAC_OK;
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

  // Validate against the device before anything is torn down, so a rejected request leaves
  // the running pipeline alone.
  if (pick_format(_device, width, height, fps) == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"%@ cannot capture %ux%u at %u fps",
                                           _device.localizedName, width, height, fps]);
  }

  scwc_mac_frame_cb callback = _frameCallback;
  void *user = _frameUser;
  [self teardown];

  _width = width;
  _height = height;
  _fps = fps;
  _codec = codec;
  _bitrateKbps = bitrate;
  [self setCallback:callback user:user];

  int32_t code = [self buildPipeline];
  if (code != SCWC_MAC_OK) {
    [self teardown];
    return code;
  }
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

/// Stops the session and the encoder; afterwards no callback can reach Rust.
- (void)teardown {
  {
    std::lock_guard<std::mutex> guard(_callbackLock);
    _shutDown = true;
    _frameCallback = NULL;
    _frameUser = NULL;
  }

  if (_output != nil) {
    [_output setSampleBufferDelegate:nil queue:nil];
  }
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
  // Drain the delegate queue: a `captureOutput:` already enqueued would run otherwise.
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

/// Turns on the requested format and frame rate, then starts session and encoder.
- (int32_t)buildPipeline {
  if (_device == nil) {
    return fail(SCWC_MAC_ERR_NO_CAMERA, @"the capture device is gone");
  }

  AVCaptureDeviceFormat *format = pick_format(_device, _width, _height, _fps);
  if (format == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"%@ cannot capture %ux%u at %u fps",
                                           _device.localizedName, _width, _height, _fps]);
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
    CMTime duration = frame_duration(format, _fps);
    [_device setActiveVideoMinFrameDuration:duration];
    [_device setActiveVideoMaxFrameDuration:duration];
  } @catch (NSException *exception) {
    [_device unlockForConfiguration];
    return fail(SCWC_MAC_ERR_UNSUPPORTED,
                [NSString stringWithFormat:@"%@ rejected %ux%u at %u fps: %@",
                                           _device.localizedName, _width, _height, _fps,
                                           exception.reason]);
  }
  [_device unlockForConfiguration];
  _format = format;

  _session = [[AVCaptureSession alloc] init];

  AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:_device error:&error];
  if (input == nil) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, [NSString stringWithFormat:@"cannot capture from %@: %@",
                                                                    _device.localizedName,
                                                                    error.localizedDescription]);
  }
  if (![_session canAddInput:input]) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, @"the capture session refused the device input");
  }
  [_session addInput:input];

  AVCaptureVideoDataOutput *output = [[AVCaptureVideoDataOutput alloc] init];
  // Late frames are dropped by the session rather than queued; the encoder is the throttle.
  [output setAlwaysDiscardsLateVideoFrames:YES];
  [output setVideoSettings:@{
    (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange),
  }];
  [output setSampleBufferDelegate:self queue:_delegateQueue];
  if (![_session canAddOutput:output]) {
    return fail(SCWC_MAC_ERR_UNSUPPORTED, @"the capture session refused the video output");
  }
  [_session addOutput:output];
  _output = output;

  int32_t code = [self createEncoder];
  if (code != SCWC_MAC_OK) {
    return code;
  }

  @try {
    [_session startRunning];
  } @catch (NSException *exception) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"capture session failed to start: %@",
                                                            exception.reason]);
  }
  if (![_session isRunning]) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"capture session did not start for %@",
                                                            _device.localizedName]);
  }
  return SCWC_MAC_OK;
}

/// Creates the VideoToolbox encoder for the current parameters.
- (int32_t)createEncoder {
  CMVideoCodecType codec = _codec == 1 ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264;
  OSStatus status = VTCompressionSessionCreate(kCFAllocatorDefault, (int32_t)_width,
                                                (int32_t)_height, codec, NULL, NULL,
                                                kCFAllocatorDefault, vt_output_callback,
                                                (__bridge void *)self, &_encoder);
  if (status != noErr || _encoder == NULL) {
    return fail(SCWC_MAC_ERR_OS, [NSString stringWithFormat:@"VideoToolbox refused %ux%u (%d)",
                                                            _width, _height, (int)status]);
  }

  set_property(_encoder, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
  // A GOP of two seconds bounds how long a late joiner waits for a keyframe.
  set_property(_encoder, kVTCompressionPropertyKey_MaxKeyFrameInterval,
               (__bridge CFTypeRef)@(2 * _fps));
  set_property(_encoder, kVTCompressionPropertyKey_AverageBitRate,
               (__bridge CFTypeRef)@(_bitrateKbps * 1000u));
  set_property(_encoder, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFTypeRef)@(_fps));
  // No B-frames: reordering would buy little bandwidth and cost latency.
  set_property(_encoder, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
  set_property(_encoder, kVTCompressionPropertyKey_ProfileLevel,
               _codec == 1 ? kVTProfileLevel_HEVC_Main_AutoLevel
                           : kVTProfileLevel_H264_High_AutoLevel);

  status = VTCompressionSessionPrepareToEncodeFrames(_encoder);
  if (status != noErr) {
    return fail(SCWC_MAC_ERR_OS,
                [NSString stringWithFormat:@"VideoToolbox could not prepare the encoder (%d)",
                                           (int)status]);
  }
  return SCWC_MAC_OK;
}

/// Keeps the newest frame for `take_photo` and hands it to the encoder.
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

  VTCompressionSessionRef encoder = _encoder;
  if (encoder == NULL) {
    return;
  }
  CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
  OSStatus status = VTCompressionSessionEncodeFrame(encoder, pixel_buffer, pts, kCMTimeInvalid,
                                                     NULL, NULL, NULL);
  if (status != noErr) {
    NSLog(@"[scwc-capture] encoding a frame failed: %d", (int)status);
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
    NSLog(@"[scwc-capture] the encoder reported status %d", (int)status);
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
      begin_cb(user, device.uniqueID.UTF8String, device.localizedName.UTF8String);

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
