import 'dart:async';
import 'dart:collection';

import 'package:app/src/rust/api.dart';
import 'package:app/src/rust/frb_generated.dart';
import 'package:flutter/material.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'src/permissions.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  runApp(const MyApp());
}

/// Root of the operator tool; owns the single [AgentState].
class MyApp extends StatefulWidget {
  const MyApp({super.key});

  @override
  State<MyApp> createState() => _MyAppState();
}

class _MyAppState extends State<MyApp> {
  final AgentState _state = AgentState();

  @override
  void initState() {
    super.initState();
    unawaited(_state.load());
  }

  @override
  void dispose() {
    _state.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'SC Webcam',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: HomePage(state: _state),
    );
  }
}

/// One timestamped line of the event log.
class LogEntry {
  const LogEntry(this.time, this.message);

  final DateTime time;
  final String message;
}

/// Resolutions of [camera], largest first, as announced by the platform.
List<String> _resolutionsOf(CameraInfoDto camera) => [
  for (final preset in camera.presets) preset.$1,
];

/// Frame rates the platform announces for one `(camera, resolution)` pair.
List<int> _frameRatesOf(CameraInfoDto camera, String? resolution) {
  if (resolution == null) return const [];
  for (final preset in camera.presets) {
    if (preset.$1 == resolution) return preset.$2.toList();
  }
  return const [];
}

/// Everything the single page shows: connection fields, camera selection, agent status, event log.
class AgentState extends ChangeNotifier {
  static const List<String> codecs = ['h264', 'h265'];
  static const int defaultBitrateKbps = 30000;
  static const int _maxLogEntries = 100;

  static const String _kServer = 'server';
  static const String _kDeviceId = 'device_id';
  static const String _kToken = 'token';
  static const String _kCodec = 'codec';
  static const String _kBitrate = 'bitrate_kbps';
  static const String _kCameraId = 'camera_id';
  static const String _kResolution = 'resolution';
  static const String _kFps = 'fps';

  final TextEditingController serverController = TextEditingController();
  final TextEditingController deviceIdController = TextEditingController();
  final TextEditingController tokenController = TextEditingController();
  final TextEditingController bitrateController = TextEditingController(
    text: '$defaultBitrateKbps',
  );

  final List<LogEntry> _log = [];

  String _codec = codecs.first;
  List<CameraInfoDto> _cameras = const [];
  CameraInfoDto? _camera;
  String? _resolution;
  int? _fps;
  AgentEvent? _status;
  BigInt? _sentFrames;
  BigInt? _droppedFrames;
  bool _running = false;
  bool _busy = false;
  bool _disposed = false;
  bool _restoring = false;

  SharedPreferences? _prefs;
  StreamSubscription<AgentEvent>? _events;
  String? _savedCameraId;
  String? _savedResolution;
  int? _savedFps;

  AgentState() {
    for (final controller in [
      serverController,
      deviceIdController,
      tokenController,
      bitrateController,
    ]) {
      controller.addListener(_onFieldChanged);
    }
  }

  /// Restores persisted settings, subscribes to the agent's events and lists the cameras.
  Future<void> load() async {
    _prefs = await SharedPreferences.getInstance();
    _restore();
    _notify();
    _events = agentEvents().listen(_onEvent, onError: _onStreamError);
    unawaited(refreshCameras());
  }

  @override
  void dispose() {
    _disposed = true;
    final events = _events;
    _events = null;
    if (events != null) {
      unawaited(events.cancel());
    }
    serverController.dispose();
    deviceIdController.dispose();
    tokenController.dispose();
    bitrateController.dispose();
    super.dispose();
  }

  /// Cameras of the host, empty until [refreshCameras] ran.
  List<CameraInfoDto> get cameras => _cameras;

  CameraInfoDto? get camera => _camera;

  String? get resolution => _resolution;

  int? get fps => _fps;

  String get codec => _codec;

  /// Parsed bitrate, null while the field is not a positive number.
  int? get bitrate {
    final value = int.tryParse(bitrateController.text.trim());
    return value != null && value > 0 ? value : null;
  }

  /// Resolutions offered by [camera].
  List<String> get resolutions =>
      _camera == null ? const [] : _resolutionsOf(_camera!);

  /// Frame rates offered by the selected `(camera, resolution)` pair.
  List<int> get frameRates =>
      _camera == null ? const [] : _frameRatesOf(_camera!, _resolution);

  bool get running => _running;

  /// True while [start] or [stop] is in flight.
  bool get busy => _busy;

  /// Newest first, at most [_maxLogEntries] lines.
  List<LogEntry> get log => UnmodifiableListView(_log);

  /// The last state-changing event; stats only feed the counters and are not kept here.
  AgentEvent? get lastStatus => _status;

  bool get hasError => _status is AgentEvent_Error;

  bool get isRecording => _status is AgentEvent_Recording;

  /// Human-readable form of [lastStatus].
  String get statusText => switch (_status) {
    null => 'Idle',
    AgentEvent_Connecting() => 'Connecting',
    AgentEvent_Online() => 'Online',
    AgentEvent_Recording(:final streamId) => 'Recording ($streamId)',
    AgentEvent_RecordingStopped() => 'Online, stream stopped',
    AgentEvent_Reconnecting(:final attempt) =>
      'Reconnecting (attempt $attempt)',
    AgentEvent_Error(:final message) => 'Error: $message',
    AgentEvent_Stats() => 'Idle',
  };

  String get statsText => _sentFrames == null
      ? 'No stats yet'
      : 'Sent $_sentFrames frames, dropped ${_droppedFrames ?? BigInt.zero}';

  bool get canStart =>
      !_busy &&
      !_running &&
      serverController.text.trim().isNotEmpty &&
      deviceIdController.text.trim().isNotEmpty &&
      tokenController.text.isNotEmpty &&
      _camera != null &&
      _resolution != null &&
      _fps != null &&
      bitrate != null;

  /// Lists cameras and re-applies the persisted selection.
  ///
  /// The runtime camera permission is asked for first: on Android the platform needs the user's
  /// answer before the capture backend may open a camera, and asking here — on the path that
  /// every start-up and every refresh goes through — means the dialog is long gone by the time
  /// an operator presses Start.
  Future<void> refreshCameras() async {
    final denial = await CameraPermission.ensureCameraPermission();
    if (denial != null) {
      _addLog(denial);
      _notify();
      return;
    }
    if (_disposed) {
      return;
    }
    final List<CameraInfoDto> cameras;
    try {
      cameras = listCameras();
    } catch (error) {
      _addLog('listCameras failed: $error');
      _notify();
      return;
    }
    // Re-apply the current pick, falling back to the persisted one on the first refresh.
    final wantedId = _camera?.id ?? _savedCameraId;
    final wantedResolution = _resolution ?? _savedResolution;
    final wantedFps = _fps ?? _savedFps;
    _cameras = cameras;
    _addLog('Found ${cameras.length} camera(s)');
    CameraInfoDto? wanted;
    for (final camera in cameras) {
      if (camera.id == wantedId) {
        wanted = camera;
        break;
      }
    }
    _applySelection(
      wanted ?? (cameras.isEmpty ? null : cameras.first),
      preferResolution: wantedResolution,
      preferFps: wantedFps,
    );
    _savedCameraId = null;
    _savedResolution = null;
    _savedFps = null;
    _persistSelection();
    _notify();
  }

  void setCodec(String codec) {
    _codec = codec;
    _persist();
    _notify();
  }

  void setCamera(String cameraId) {
    for (final camera in _cameras) {
      if (camera.id == cameraId) {
        _applySelection(camera);
        _persist();
        _notify();
        return;
      }
    }
  }

  void setResolution(String resolution) {
    _applySelection(_camera, preferResolution: resolution);
    _persist();
    _notify();
  }

  void setFps(int fps) {
    if (!frameRates.contains(fps)) return;
    _fps = fps;
    _persist();
    _notify();
  }

  /// Starts the agent with the current settings.
  Future<void> start() async {
    final camera = _camera;
    final size = _parseResolution(_resolution);
    final fps = _fps;
    final bitrateKbps = bitrate;
    if (!canStart ||
        camera == null ||
        size == null ||
        fps == null ||
        bitrateKbps == null) {
      return;
    }

    _busy = true;
    _notify();
    try {
      await startAgent(
        config: AgentConfig(
          server: serverController.text.trim(),
          deviceId: deviceIdController.text.trim(),
          token: tokenController.text.trim(),
          cameraId: camera.id,
          width: size.$1,
          height: size.$2,
          fps: fps,
          codec: _codec,
          bitrateKbps: bitrateKbps,
        ),
      );
      _running = true;
      _addLog(
        'Agent started: ${camera.name} $_resolution @ ${fps}fps '
        '$_codec ${bitrateKbps}kbps',
      );
    } catch (error) {
      _addLog('Start failed: $error');
    } finally {
      _busy = false;
      _notify();
    }
  }

  /// Asks the agent to wind down.
  Future<void> stop() async {
    _busy = true;
    _notify();
    try {
      await stopAgent();
      _running = false;
      _addLog('Agent stopped');
    } catch (error) {
      _addLog('Stop failed: $error');
    } finally {
      _busy = false;
      _notify();
    }
  }

  void _onEvent(AgentEvent event) {
    switch (event) {
      case AgentEvent_Stats(:final sentFrames, :final droppedFrames):
        _sentFrames = sentFrames;
        _droppedFrames = droppedFrames;
      case AgentEvent_Connecting():
        _running = true;
        _status = event;
        _addLog('connecting');
      case AgentEvent_Online():
        _running = true;
        _status = event;
        _addLog('online');
      case AgentEvent_Recording(:final streamId):
        _running = true;
        _status = event;
        _addLog('recording $streamId');
      case AgentEvent_RecordingStopped():
        _running = true;
        _status = event;
        _addLog('recording stopped');
      case AgentEvent_Reconnecting(:final attempt):
        _running = true;
        _status = event;
        _addLog('reconnecting (attempt $attempt)');
      case AgentEvent_Error(:final message):
        _running = false;
        _status = event;
        _addLog('error: $message');
    }
    _notify();
  }

  void _onStreamError(Object error) {
    _addLog('event stream error: $error');
    _notify();
  }

  /// Picks the camera plus a resolution/frame rate pair that exists on it, preferring the
  /// persisted (or previously chosen) values when the camera still offers them.
  void _applySelection(
    CameraInfoDto? camera, {
    String? preferResolution,
    int? preferFps,
  }) {
    _camera = camera;
    if (camera == null) {
      _resolution = null;
      _fps = null;
      return;
    }
    final resolutions = _resolutionsOf(camera);
    _resolution = resolutions.contains(preferResolution)
        ? preferResolution
        : (resolutions.isEmpty ? null : resolutions.first);
    final rates = _frameRatesOf(camera, _resolution);
    _fps = rates.contains(preferFps)
        ? preferFps
        : (rates.isEmpty ? null : rates.first);
  }

  void _onFieldChanged() {
    // The controllers are written during _restore; persisting half-restored fields would wipe
    // the stored ones.
    if (!_restoring) _persist();
    _notify();
  }

  void _restore() {
    final prefs = _prefs!;
    _restoring = true;
    try {
      serverController.text = prefs.getString(_kServer) ?? '';
      deviceIdController.text = prefs.getString(_kDeviceId) ?? '';
      tokenController.text = prefs.getString(_kToken) ?? '';
      bitrateController.text =
          '${prefs.getInt(_kBitrate) ?? defaultBitrateKbps}';
      final codec = prefs.getString(_kCodec);
      if (codec != null && codecs.contains(codec)) {
        _codec = codec;
      }
      _savedCameraId = prefs.getString(_kCameraId);
      _savedResolution = prefs.getString(_kResolution);
      _savedFps = prefs.getInt(_kFps);
    } finally {
      _restoring = false;
    }
  }

  void _persist() {
    final prefs = _prefs;
    if (prefs == null) return;
    unawaited(prefs.setString(_kServer, serverController.text.trim()));
    unawaited(prefs.setString(_kDeviceId, deviceIdController.text.trim()));
    // Stored so a restart is painless; never written to the log.
    unawaited(prefs.setString(_kToken, tokenController.text));
    unawaited(prefs.setString(_kCodec, _codec));
    unawaited(
      prefs.setInt(
        _kBitrate,
        int.tryParse(bitrateController.text.trim()) ?? defaultBitrateKbps,
      ),
    );
    _persistSelection();
  }

  void _persistSelection() {
    final prefs = _prefs;
    final camera = _camera;
    // A host without cameras (or before the first refresh) must not drop the stored selection.
    if (prefs == null || camera == null) return;
    unawaited(prefs.setString(_kCameraId, camera.id));
    final resolution = _resolution;
    if (resolution == null) {
      unawaited(prefs.remove(_kResolution));
    } else {
      unawaited(prefs.setString(_kResolution, resolution));
    }
    final fps = _fps;
    if (fps == null) {
      unawaited(prefs.remove(_kFps));
    } else {
      unawaited(prefs.setInt(_kFps, fps));
    }
  }

  void _addLog(String message) {
    _log.insert(0, LogEntry(DateTime.now(), message));
    if (_log.length > _maxLogEntries) {
      _log.removeRange(_maxLogEntries, _log.length);
    }
  }

  void _notify() {
    if (!_disposed) notifyListeners();
  }

  static (int, int)? _parseResolution(String? resolution) {
    final parts = resolution?.split('x');
    if (parts == null || parts.length != 2) return null;
    final width = int.tryParse(parts[0]);
    final height = int.tryParse(parts[1]);
    if (width == null || height == null) return null;
    return (width, height);
  }
}

class HomePage extends StatelessWidget {
  const HomePage({super.key, required this.state});

  final AgentState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) => Scaffold(
        appBar: AppBar(title: const Text('SC Webcam')),
        body: Padding(
          padding: const EdgeInsets.all(12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Expanded(
                flex: 5,
                child: SingleChildScrollView(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      _connectionCard(context),
                      _cameraCard(context),
                      _controls(context),
                      _statusCard(context),
                    ],
                  ),
                ),
              ),
              const SizedBox(height: 12),
              Expanded(flex: 4, child: _logCard(context)),
            ],
          ),
        ),
      ),
    );
  }

  Widget _connectionCard(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('Connection', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            TextField(
              controller: state.serverController,
              decoration: const InputDecoration(
                labelText: 'Server URL',
                hintText: 'https://smartclass.example.edu',
              ),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: state.deviceIdController,
              decoration: const InputDecoration(labelText: 'Device id'),
            ),
            const SizedBox(height: 8),
            TextField(
              controller: state.tokenController,
              obscureText: true,
              decoration: const InputDecoration(labelText: 'Token'),
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Expanded(
                  child: DropdownButtonFormField<String>(
                    initialValue: state.codec,
                    decoration: const InputDecoration(labelText: 'Codec'),
                    items: [
                      for (final codec in AgentState.codecs)
                        DropdownMenuItem(value: codec, child: Text(codec)),
                    ],
                    onChanged: (value) {
                      if (value != null) state.setCodec(value);
                    },
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: TextField(
                    controller: state.bitrateController,
                    keyboardType: TextInputType.number,
                    decoration: InputDecoration(
                      labelText: 'Bitrate (kbps)',
                      errorText: state.bitrate == null
                          ? 'Enter a number'
                          : null,
                    ),
                  ),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }

  Widget _cameraCard(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Expanded(
                  child: Text(
                    'Camera',
                    style: Theme.of(context).textTheme.titleMedium,
                  ),
                ),
                TextButton.icon(
                  onPressed: state.refreshCameras,
                  icon: const Icon(Icons.refresh, size: 18),
                  label: const Text('Refresh cameras'),
                ),
              ],
            ),
            const SizedBox(height: 8),
            DropdownButtonFormField<String>(
              initialValue: state.camera?.id,
              decoration: InputDecoration(
                labelText: 'Camera',
                hintText: state.cameras.isEmpty
                    ? 'Refresh to list cameras'
                    : null,
              ),
              items: [
                for (final camera in state.cameras)
                  DropdownMenuItem(value: camera.id, child: Text(camera.name)),
              ],
              onChanged: (value) {
                if (value != null) state.setCamera(value);
              },
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Expanded(
                  child: DropdownButtonFormField<String>(
                    initialValue: state.resolution,
                    decoration: const InputDecoration(labelText: 'Resolution'),
                    items: [
                      for (final resolution in state.resolutions)
                        DropdownMenuItem(
                          value: resolution,
                          child: Text(resolution),
                        ),
                    ],
                    onChanged: (value) {
                      if (value != null) state.setResolution(value);
                    },
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: DropdownButtonFormField<int>(
                    initialValue: state.fps,
                    decoration: const InputDecoration(labelText: 'FPS'),
                    items: [
                      for (final fps in state.frameRates)
                        DropdownMenuItem(value: fps, child: Text('$fps')),
                    ],
                    onChanged: (value) {
                      if (value != null) state.setFps(value);
                    },
                  ),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }

  Widget _controls(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        children: [
          FilledButton.icon(
            onPressed: state.busy
                ? null
                : (state.running ? state.stop : state.start),
            icon: Icon(state.running ? Icons.stop : Icons.play_arrow),
            label: Text(state.running ? 'Stop' : 'Start'),
          ),
          const SizedBox(width: 12),
          if (state.busy)
            const SizedBox(
              width: 16,
              height: 16,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
          if (!state.running && !state.busy && !state.canStart)
            const Expanded(
              child: Text(
                'Fill in the connection and pick a camera to start',
                style: TextStyle(fontSize: 12),
              ),
            ),
        ],
      ),
    );
  }

  Widget _statusCard(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = state.hasError
        ? scheme.error
        : state.isRecording
        ? Colors.green.shade700
        : scheme.onSurface;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('Status', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            Row(
              children: [
                Icon(Icons.circle, size: 12, color: color),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    state.statusText,
                    style: TextStyle(fontWeight: FontWeight.w600, color: color),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 4),
            Text(state.statsText),
          ],
        ),
      ),
    );
  }

  Widget _logCard(BuildContext context) {
    final entries = state.log;
    return Card(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 12, 12, 4),
            child: Text(
              'Event log',
              style: Theme.of(context).textTheme.titleMedium,
            ),
          ),
          Expanded(
            child: entries.isEmpty
                ? const Center(child: Text('No events yet'))
                : ListView.builder(
                    padding: const EdgeInsets.symmetric(horizontal: 12),
                    itemCount: entries.length,
                    itemBuilder: (context, index) {
                      final entry = entries[index];
                      return Padding(
                        padding: const EdgeInsets.symmetric(vertical: 2),
                        child: Text(
                          '${_formatTime(entry.time)}  ${entry.message}',
                          style: const TextStyle(
                            fontFamily: 'Menlo',
                            fontSize: 12,
                          ),
                        ),
                      );
                    },
                  ),
          ),
        ],
      ),
    );
  }

  static String _formatTime(DateTime time) {
    String two(int value) => value.toString().padLeft(2, '0');
    return '${two(time.hour)}:${two(time.minute)}:${two(time.second)}';
  }
}
