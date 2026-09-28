import tailwindcss from "@tailwindcss/vite"

// The desktop and mobile shell is Tauri, so this is a static SPA: there is no
// server to render on, and `nuxt generate` produces the files Tauri bundles.
export default defineNuxtConfig({
  ssr: false,
  compatibilityDate: "2026-07-29",
  // The kit's own Nuxt module (which would auto-register every `Lp*`
  // component) needs `@nuxt/kit` resolvable from the kit's directory, and the
  // local checkout we depend on does not carry it — it is a peer dep supplied
  // by whichever app consumes the kit, and a `file:` dependency resolves from
  // its real path rather than ours. Components are imported explicitly instead,
  // which the kit's README documents as the other supported way and which also
  // keeps the bundle to what this screen actually uses.
  modules: ["@nuxtjs/i18n"],
  css: ["~/assets/css/main.css"],
  vite: {
    plugins: [tailwindcss()],
    // Tauri serves the dev build over a fixed port and shows Rust panics in its
    // own console, so keep Vite's overlay from covering the app.
    server: { strictPort: true },
  },
  devServer: { host: "127.0.0.1", port: 1430 },
  i18n: {
    // No prefixes: this is an app window, not a site — there are no URLs to
    // share and nothing should depend on the locale being in the path.
    strategy: "no_prefix",
    defaultLocale: "en",
    locales: [{ code: "en", name: "English", file: "en.json" }],
    detectBrowserLanguage: {
      useCookie: true,
      cookieKey: "cursio_locale",
      redirectOn: "root",
    },
  },
  app: {
    head: {
      title: "Cursio",
      meta: [
        // A touch surface must not pan, zoom or bounce under the finger: every
        // one of those turns a deliberate drag into a gesture the page eats.
        {
          name: "viewport",
          content:
            "width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no, viewport-fit=cover",
        },
      ],
    },
  },
})
