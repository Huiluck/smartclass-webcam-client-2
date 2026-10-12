#import <CoreVideo/CoreVideo.h>
#import <FlutterMacOS/FlutterMacOS.h>

#import "ScwcPreviewPlugin.h"

/// The texture path's C ABI, exported by this pod's Rust library (`app/rust_lib/src/lib.rs`).
///
/// `scwc_preview_register_frame_callback` takes either a callback — which is called from the
/// capture thread once per captured frame, and must return promptly — or NULL, which stops the
/// path and releases the frame it was holding. `scwc_preview_latest_frame` answers with a
/// `CVPixelBufferRef` the caller owns one reference to, or NULL while no frame has been captured.
extern void scwc_preview_register_frame_callback(void (*callback)(void));
extern void *scwc_preview_latest_frame(void);

/// The callback the capture path calls when a newer frame is published; defined below, next to
/// the plugin it posts to.
static void scwc_preview_frame_ready(void);

/// The plugin instance the Rust callback posts to. One registrar means one instance, and the
/// callback outlives it only until `stop`, so the reference is weak: a notice that arrives while
/// no texture is registered is dropped.
static __weak ScwcPreviewPlugin *gPreviewPlugin = nil;

/// No texture is registered.
static const int64_t noTexture = -1;

/// The parts of the plugin the channel and the texture protocol drive, declared here so that the
/// methods below may call them in any order.
@interface ScwcPreviewPlugin ()
/// Stops the path and unregisters the texture.
- (void)stopTexture;
/// The newest frame's size as `[width, height]`, or null while none has been captured.
- (NSArray<NSNumber *> *)frameSize;
/// Tells Flutter that there is a newer frame to paint.
- (void)textureFrameAvailable;
@end

@implementation ScwcPreviewPlugin {
  NSObject<FlutterTextureRegistry> *_textures;
  /// The registered texture, or [`noTexture`] while none is.
  ///
  /// A registry's ids are its own: Flutter's macOS one starts at zero, so zero cannot be the
  /// "no texture" value — the first texture a client registers would look unregistered, and every
  /// `start` after it would register another one.
  int64_t _textureId;
}

+ (void)registerWithRegistrar:(NSObject<FlutterPluginRegistrar> *)registrar {
  FlutterMethodChannel *channel =
      [FlutterMethodChannel methodChannelWithName:@"scwc_preview"
                                  binaryMessenger:[registrar messenger]];
  ScwcPreviewPlugin *plugin = [[ScwcPreviewPlugin alloc] initWithTextures:[registrar textures]];
  [registrar addMethodCallDelegate:plugin channel:channel];
}

- (instancetype)initWithTextures:(NSObject<FlutterTextureRegistry> *)textures {
  self = [super init];
  if (self != nil) {
    _textures = textures;
    _textureId = noTexture;
    gPreviewPlugin = self;
  }
  return self;
}

- (void)handleMethodCall:(FlutterMethodCall *)call result:(FlutterResult)result {
  if ([call.method isEqualToString:@"start"]) {
    // Starting a texture that is already registered is the same texture: the interface may ask
    // again after a rebuild, and handing it a second one would paint two.
    if (_textureId == noTexture) {
      _textureId = [_textures registerTexture:self];
      scwc_preview_register_frame_callback(scwc_preview_frame_ready);
    }
    result(@(_textureId));
  } else if ([call.method isEqualToString:@"stop"]) {
    [self stopTexture];
    result(nil);
  } else if ([call.method isEqualToString:@"size"]) {
    // The frame's own size, so an interface can letterbox with the ratio the camera negotiated
    // rather than the one it asked for.
    result([self frameSize]);
  } else {
    result(FlutterMethodNotImplemented);
  }
}

/// Stops the path and drops the texture; a call for a texture that is already gone does nothing.
///
/// The texture is unregistered first, so Flutter can no longer ask for a buffer by the time the
/// Rust side releases the frame it holds: nothing retains a buffer that is on its way out.
- (void)stopTexture {
  if (_textureId == noTexture) {
    return;
  }
  int64_t textureId = _textureId;
  _textureId = noTexture;
  [_textures unregisterTexture:textureId];
  scwc_preview_register_frame_callback(NULL);
}

#pragma mark - FlutterTexture

/// The newest camera frame, retained for Flutter, or NULL while none has been captured.
///
/// Flutter owns the reference it is handed and releases it when it is done with the frame, which
/// is why the Rust side retains one for this call rather than lending its own.
- (CVPixelBufferRef)copyPixelBuffer {
  if (_textureId == noTexture) {
    return NULL;
  }
  CVPixelBufferRef buffer = (CVPixelBufferRef)scwc_preview_latest_frame();
  return buffer;
}

/// The newest frame's size as `[width, height]`, or null while none has been captured.
- (NSArray<NSNumber *> *)frameSize {
  CVPixelBufferRef buffer = (CVPixelBufferRef)scwc_preview_latest_frame();
  if (buffer == NULL) {
    return nil;
  }
  NSArray<NSNumber *> *size = @[ @(CVPixelBufferGetWidth(buffer)), @(CVPixelBufferGetHeight(buffer)) ];
  // The reference this call took, and the frame itself is still held by the capture path.
  CVPixelBufferRelease(buffer);
  return size;
}

/// Tells Flutter that there is a newer frame to paint.
- (void)textureFrameAvailable {
  if (_textureId == noTexture) {
    return;
  }
  [_textures textureFrameAvailable:_textureId];
}

@end

/// The callback the capture path calls when a newer frame has been published.
///
/// It runs on the capture thread, which must not wait for the interface: the notice is posted to
/// the main thread and the capture callback returns.
static void scwc_preview_frame_ready(void) {
  ScwcPreviewPlugin *plugin = gPreviewPlugin;
  if (plugin == nil) {
    return;
  }
  dispatch_async(dispatch_get_main_queue(), ^{
    [plugin textureFrameAvailable];
  });
}
