//
//  Generated file. Do not edit.
//

// clang-format off

#include "generated_plugin_registrant.h"

#include <rust_lib/scwc_preview_plugin.h>

void fl_register_plugins(FlPluginRegistry* registry) {
  g_autoptr(FlPluginRegistrar) rust_lib_registrar =
      fl_plugin_registry_get_registrar_for_plugin(registry, "ScwcPreviewPlugin");
  scwc_preview_plugin_register_with_registrar(rust_lib_registrar);
}
