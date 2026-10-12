import 'dart:async';
import 'dart:ui' as ui;

import 'package:app/src/agent_state.dart';
import 'package:app/src/rust/api.dart';
import 'package:flutter/foundation.dart';

/// The live camera picture: the newest frame of the bridge's preview stream, decoded and ready to
/// paint.
///
/// The feed follows one [AgentState] and shows only what that state can vouch for. A picture is
/// dropped the moment the pipeline behind it is replaced or torn down, a frame that lands after
/// that is not painted at all, and a picture nothing feeds any more falls back to the placeholder
/// instead of freezing, so the preview never claims to be live when it is not. Frames arrive at the
/// capture rate and decoding one is asynchronous, so a frame that lands while the previous one is
/// still decoding replaces it instead of queueing behind it.
class PreviewFeed extends ChangeNotifier {
  PreviewFeed({this.onError});

  /// Called when the frame stream fails; the picture then keeps the last frame it decoded.
  final void Function(Object error)? onError;

  /// How many frame intervals a picture may stand without a frame before it is stale.
  static const int _staleFrameIntervals = 3;

  /// Floor of the staleness timeout: decoding and delivering a frame is allowed to take a few
  /// hundred milliseconds on a busy machine without the picture being declared dead.
  static const Duration _minStaleTimeout = Duration(milliseconds: 200);

  /// Frame rate assumed until the agent reports the one it runs at.
  static const int _defaultFps = 30;

  AgentState? _state;

  /// The pipeline the picture on screen belongs to; see [AgentState.previewEpoch].
  int _epoch = 0;

  ui.Image? _image;
  PreviewFrame? _pending;
  Duration _timeout = _minStaleTimeout;
  Timer? _watchdog;
  DateTime? _lastFrame;
  bool _decoding = false;
  bool _disposed = false;
  StreamSubscription<PreviewFrame>? _frames;

  /// The newest frame of the pipeline the feed follows, or null while there is none to show.
  ui.Image? get image => _image;

  /// Subscribes to the preview stream and to [state], which decides which frames may be shown;
  /// calling it again is a no-op.
  void start(AgentState state) {
    if (_state != null) return;
    _state = state;
    state.addListener(_onStateChanged);
    _frames = previewStream().listen(_onFrame, onError: _onStreamError);
    _onStateChanged();
  }

  @override
  void dispose() {
    _disposed = true;
    _state?.removeListener(_onStateChanged);
    _state = null;
    unawaited(_frames?.cancel());
    _frames = null;
    _watchdog?.cancel();
    _watchdog = null;
    _lastFrame = null;
    _pending = null;
    _dropImage();
    super.dispose();
  }

  /// True while frames may be shown: only the running pipeline is allowed to fill the preview.
  bool get _accepting {
    final state = _state;
    return !_disposed &&
        state != null &&
        state.running &&
        state.previewEpoch == _epoch;
  }

  /// Follows the agent: the picture goes as soon as the pipeline behind it is replaced or torn
  /// down, and the watchdog that takes a picture down once its frames stop is kept on the rate the
  /// followed pipeline runs at.
  void _onStateChanged() {
    final state = _state;
    if (state == null) return;

    if (!state.running || state.previewEpoch != _epoch) {
      _epoch = state.previewEpoch;
      _drop();
      if (!state.running) return;
    }
    _watch(state.captureFps);
  }

  void _onFrame(PreviewFrame frame) {
    // A frame of a pipeline the feed no longer follows — one that was already on its way when the
    // run stopped or the sink was cleared — is dropped rather than painted.
    if (!_accepting) return;
    _lastFrame = DateTime.now();
    _pending = frame;
    if (!_decoding) _decodeNext();
  }

  void _onStreamError(Object error) {
    // The stream carries frames rather than state, so a failure of its own leaves the picture up
    // until the staleness timeout takes it down; the report belongs in the event log.
    onError?.call(error);
  }

  /// Decodes the newest frame that arrived, unless one is already decoding.
  void _decodeNext() {
    final frame = _pending;
    if (frame == null) {
      _decoding = false;
      return;
    }
    _pending = null;
    _decoding = true;
    // The client converts and scales on the Rust side, so these pixels are painted as they are.
    ui.decodeImageFromPixels(
      frame.rgba,
      frame.width,
      frame.height,
      ui.PixelFormat.rgba8888,
      _onDecoded,
      rowBytes: frame.width * 4,
    );
  }

  void _onDecoded(ui.Image image) {
    _decoding = false;
    if (!_accepting) {
      // The pipeline this frame belongs to is gone; it must not be painted over the one that
      // replaced it.
      _pending = null;
      image.dispose();
      return;
    }
    final previous = _image;
    _image = image;
    // Whatever painted the previous frame holds a handle of its own on it, so this one goes as soon
    // as it is replaced rather than piling up at the capture rate.
    previous?.dispose();
    notifyListeners();
    // A frame that landed while this one decoded is newer than what is painted now.
    if (_pending != null) _decodeNext();
  }

  /// Keeps one watchdog armed for the frame rate the followed pipeline runs at.
  void _watch(int? fps) {
    final timeout = _staleTimeout(fps);
    if (_watchdog != null && timeout == _timeout) return;
    _timeout = timeout;
    _armWatchdog();
  }

  void _armWatchdog() {
    _watchdog?.cancel();
    _watchdog = Timer(_timeout, _checkStale);
  }

  /// Takes a picture down that no frame refreshed for [_timeout], and keeps watching otherwise: a
  /// pipeline that has not produced its first frame yet or one that is merely quiet stays watched,
  /// while a pipeline that is gone is not.
  void _checkStale() {
    _watchdog = null;
    if (!_accepting) return;

    final last = _lastFrame;
    if (_image != null && last != null && DateTime.now().difference(last) >= _timeout) {
      _drop();
      return;
    }
    _armWatchdog();
  }

  /// Takes the picture off screen: the handle goes, the frames queued behind it go, and the picture
  /// is forgotten until a pipeline is followed again.
  void _drop() {
    _watchdog?.cancel();
    _watchdog = null;
    _lastFrame = null;
    _pending = null;
    if (_image == null) return;
    _dropImage();
    notifyListeners();
  }

  void _dropImage() {
    final image = _image;
    _image = null;
    image?.dispose();
  }

  /// A few frame intervals of the running pipeline, never below [_minStaleTimeout].
  Duration _staleTimeout(int? fps) {
    final interval = (1000 / (fps ?? _defaultFps)).ceil();
    final timeout = Duration(milliseconds: interval * _staleFrameIntervals);
    return timeout < _minStaleTimeout ? _minStaleTimeout : timeout;
  }
}
