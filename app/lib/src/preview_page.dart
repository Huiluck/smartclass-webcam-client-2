import 'package:app/src/agent_state.dart';
import 'package:app/src/preview_feed.dart';
import 'package:app/src/rust/api.dart';
import 'package:flutter/material.dart';

/// The main page: the live camera picture and the agent's status.
class PreviewPage extends StatelessWidget {
  const PreviewPage({super.key, required this.state, required this.preview});

  final AgentState state;
  final PreviewFeed preview;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) => Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Expanded(
              child: PreviewView(
                preview: preview,
                // The status line stands in for the picture whenever there is none: before the
                // agent runs, while the camera is coming up, and once it stopped.
                placeholder: state.statusText,
              ),
            ),
            const SizedBox(height: 12),
            StatusCard(state: state),
          ],
        ),
      ),
    );
  }
}

/// The camera picture: the newest decoded frame centred with its aspect preserved, or a dark
/// placeholder with the agent's status while there is none.
///
/// The widget also tells the client which size to scale frames to, following its own layout, so a
/// window resize reaches the capture path within a frame.
class PreviewView extends StatefulWidget {
  const PreviewView({super.key, required this.preview, required this.placeholder});

  final PreviewFeed preview;

  /// Line shown over the dark placeholder while no frame is on screen.
  final String placeholder;

  @override
  State<PreviewView> createState() => _PreviewViewState();
}

class _PreviewViewState extends State<PreviewView> {
  int? _width;
  int? _height;

  @override
  void dispose() {
    // Nothing shows frames any more; 0x0 asks the capture path to stop converting them.
    setPreviewTargetSize(width: 0, height: 0);
    super.dispose();
  }

  /// Asks the client to scale frames to `width` x `height` pixels, once per size.
  ///
  /// A plain bridge call, not framework state: it only changes what the Rust side sends next.
  void _setTargetSize(int width, int height) {
    if (width == _width && height == _height) return;
    _width = width;
    _height = height;
    setPreviewTargetSize(width: width, height: height);
  }

  @override
  Widget build(BuildContext context) {
    final ratio = MediaQuery.devicePixelRatioOf(context);
    return LayoutBuilder(
      builder: (context, constraints) {
        // The frames are scaled for the pixels this area is painted with; the UI only centres what
        // it gets.
        _setTargetSize(
          (constraints.maxWidth * ratio).round(),
          (constraints.maxHeight * ratio).round(),
        );
        return ColoredBox(
          color: Colors.black,
          child: ListenableBuilder(
            listenable: widget.preview,
            builder: (context, _) {
              final image = widget.preview.image;
              if (image == null) {
                return Center(
                  child: Text(
                    widget.placeholder,
                    style: const TextStyle(color: Colors.white70),
                  ),
                );
              }
              // The frame carries the aspect ratio of the camera, so `contain` centres it and
              // keeps it whole.
              return SizedBox.expand(
                child: RawImage(image: image, fit: BoxFit.contain),
              );
            },
          ),
        );
      },
    );
  }
}

/// What the agent is doing: its last state, the frame counters and the parameters in use.
class StatusCard extends StatelessWidget {
  const StatusCard({super.key, required this.state});

  final AgentState state;

  @override
  Widget build(BuildContext context) {
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
            if (state.appliedText != null) ...[
              const SizedBox(height: 4),
              Text(state.appliedText!),
            ],
          ],
        ),
      ),
    );
  }
}
