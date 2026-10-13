#import <Flutter/Flutter.h>

/// Paints the camera's own frames in a Flutter texture.
///
/// Registered by the generated plugin registrant on iOS; the interface starts and stops it
/// through the `scwc_preview` method channel (see `app/lib/src/preview_texture.dart`), and the
/// pixels never pass through Dart. Declared in a header so that the registrant can name the
/// class.
@interface ScwcPreviewPlugin : NSObject <FlutterPlugin, FlutterTexture>
@end
