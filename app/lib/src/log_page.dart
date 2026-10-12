import 'package:app/src/agent_state.dart';
import 'package:flutter/material.dart';

/// The event log as its own page, newest first.
class LogPage extends StatelessWidget {
  const LogPage({super.key, required this.state});

  final AgentState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) {
        final entries = state.log;
        return Padding(
          padding: const EdgeInsets.all(12),
          child: Card(
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
                  // The log is there to be copied out of, so its lines can be selected and
                  // dragged over; a selection spans as many of them as the user takes.
                  child: SelectionArea(
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
                ),
              ],
            ),
          ),
        );
      },
    );
  }

  static String _formatTime(DateTime time) {
    String two(int value) => value.toString().padLeft(2, '0');
    return '${two(time.hour)}:${two(time.minute)}:${two(time.second)}';
  }
}
