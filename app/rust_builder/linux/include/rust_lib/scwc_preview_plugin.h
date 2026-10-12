// The camera preview as a Flutter texture, painted from frames the capture path published.
//
// The interface talks to `scwc_preview` — `start` registers the texture and answers its id, `stop`
// unregisters it, `size` answers the shape of the frame it paints — and the plugin tells the engine
// when a newer frame is waiting. Nothing crosses into Dart, so the picture costs the conversion the
// capture path already pays and no more.
//
// The plugin registrant the Flutter tooling generates calls
// `scwc_preview_plugin_register_with_registrar` at startup.

#ifndef FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_
#define FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_

#include <flutter_linux/flutter_linux.h>

G_BEGIN_DECLS

#ifdef FLUTTER_PLUGIN_IMPL
#define FLUTTER_PLUGIN_EXPORT __attribute__((visibility("default")))
#else
#define FLUTTER_PLUGIN_EXPORT
#endif

typedef struct _ScwcPreviewPlugin ScwcPreviewPlugin;
typedef struct {
  GObjectClass parent_class;
} ScwcPreviewPluginClass;

FLUTTER_PLUGIN_EXPORT GType scwc_preview_plugin_get_type();

FLUTTER_PLUGIN_EXPORT void scwc_preview_plugin_register_with_registrar(
    FlPluginRegistrar* registrar);

G_END_DECLS

#endif  // FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_
