import 'dart:io';

import 'package:flutter/services.dart';

/// The camera's own frames, painted by the platform's renderer instead of by Dart.
///
/// On macOS the capture pipeline publishes the buffer AVFoundation produced and the plugin in
/// `rust_builder/macos` hands that same buffer to a Flutter texture, so a preview costs neither a
/// conversion nor a copy of the picture — and the interface paints it at the rate the camera
/// captures, with [Texture] scaling and centring it like any other image.
///
/// The stream path ([PreviewFeed]) stays the way every other platform shows a preview: it carries
/// finished RGBA frames through the bridge, which is more work per frame but needs no platform
/// support.
class PreviewTexture {
  PreviewTexture._();

  /// The channel the plugin registers in `ScwcPreviewPlugin.m`.
  static const MethodChannel _channel = MethodChannel('scwc_preview');

  /// Whether this platform paints native frames: macOS, where the plugin and the capture shim
  /// both live. Everywhere else the stream path is the preview.
  static bool get available => Platform.isMacOS;

  /// Registers a texture and starts publishing frames into it, answering its id — or `null` when
  /// the platform has no such path, or the plugin refused.
  ///
  /// The same texture comes back when it is already registered, so a caller may start on every
  /// rebuild without leaking textures.
  static Future<int?> start() async {
    if (!available) {
      return null;
    }
    try {
      return await _channel.invokeMethod<int>('start');
    } on PlatformException {
      // No texture means no preview here; the caller falls back to the stream.
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  /// The size of the frames the camera actually produces, or `null` while none has arrived.
  ///
  /// A camera may negotiate a frame size other than the one that was asked for, and an interface
  /// that letterboxes with the negotiated ratio shows the picture undistorted.
  static Future<(int, int)?> size() async {
    if (!available) {
      return null;
    }
    try {
      final size = await _channel.invokeMethod<List<Object?>>('size');
      if (size == null || size.length != 2) {
        return null;
      }
      final width = size[0];
      final height = size[1];
      if (width is! int || height is! int || width <= 0 || height <= 0) {
        return null;
      }
      return (width, height);
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  /// Unregisters the texture: the preview stops and the frame pipeline is released.
  static Future<void> stop() async {
    if (!available) {
      return;
    }
    try {
      await _channel.invokeMethod<void>('stop');
    } on PlatformException {
      // Nothing is left to stop if the plugin is not there.
    } on MissingPluginException {
      // As above.
    }
  }
}
