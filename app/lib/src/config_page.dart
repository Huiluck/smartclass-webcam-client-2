import 'package:app/src/agent_state.dart';
import 'package:flutter/material.dart';

/// Everything the operator configures: the server identity, the camera, the capture parameters and
/// the controls over a running agent.
class ConfigPage extends StatelessWidget {
  const ConfigPage({super.key, required this.state});

  final AgentState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) => SingleChildScrollView(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _connectionCard(context),
            _cameraCard(context),
            _controls(context),
          ],
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
            Text(
              'Connection',
              style: Theme.of(context).textTheme.titleMedium,
            ),
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
                      // Empty means the client derives one; the hint says which.
                      hintText: state.bitrateHint,
                      errorText: state.bitrateInvalid ? 'Enter a number' : null,
                    ),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 8),
            Align(
              alignment: Alignment.centerRight,
              child: FilledButton.tonalIcon(
                onPressed: state.saveAndReconnect,
                icon: const Icon(Icons.save, size: 18),
                label: const Text('保存并重连'),
              ),
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
          // The only control over a live connection: there is no start button, a launch with a
          // stored identity connects by itself and 保存并重连 connects after a configuration.
          FilledButton.icon(
            onPressed: state.running && !state.busy ? state.stop : null,
            icon: const Icon(Icons.stop),
            label: const Text('Stop'),
          ),
          const SizedBox(width: 12),
          FilledButton.tonalIcon(
            onPressed: state.canApply ? state.apply : null,
            icon: const Icon(Icons.check, size: 18),
            label: const Text('Apply'),
          ),
          const SizedBox(width: 12),
          if (state.busy)
            const SizedBox(
              width: 16,
              height: 16,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
        ],
      ),
    );
  }
}
