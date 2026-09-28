/*
 * The kit's Nuxt module, run with this app's own `@nuxt/kit`.
 *
 * `@leavepulse/ui/nuxt` does exactly this, but it imports `@nuxt/kit` from the
 * kit's directory, and a `file:` checkout carries no copy of that peer
 * dependency — Nuxt fails to load the module before it runs. The work itself is
 * two calls, so it is done here instead: register every component from its own
 * dist chunk (a page bundles only what it uses, exactly as the kit's module
 * does), and install the theme plugin.
 *
 * The component list is the kit's dist directory rather than its generated
 * `COMPONENT_NAMES`, which the package does not export: the directory is what
 * that list is generated from, so a new kit component appears here with no
 * change to this file.
 */
import { readdirSync } from "node:fs"
import { dirname, join } from "node:path"
import { createRequire } from "node:module"
import { addComponent, addPlugin, createResolver, defineNuxtModule } from "@nuxt/kit"

export default defineNuxtModule({
  meta: { name: "cursio-leavepulse-ui" },
  setup() {
    const require = createRequire(import.meta.url)
    const kit = dirname(require.resolve("@leavepulse/ui/package.json"))
    const components = readdirSync(join(kit, "dist", "components"))
      .filter((file) => file.endsWith(".vue.js"))
      .map((file) => file.slice(0, -".vue.js".length))
    for (const name of components) {
      addComponent({
        name,
        export: "default",
        filePath: `@leavepulse/ui/dist/components/${name}.vue.js`,
      })
    }
    addPlugin({ src: createResolver(import.meta.url).resolve("./theme.client"), mode: "client" })
  },
})
