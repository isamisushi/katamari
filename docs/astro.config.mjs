// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// https://astro.build/config
export default defineConfig({
  site: "https://isamisushi.github.io",
  base: "/katamari",
  integrations: [
    starlight({
      title: "katamari",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/isamisushi/katamari",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/isamisushi/katamari/edit/main/docs/",
      },
      sidebar: [
        { label: "Installation", slug: "installation" },
        { label: "Quickstart", slug: "quickstart" },
        { label: "Review units", slug: "review-units" },
        { label: "Keybindings", slug: "keybindings" },
        { label: "Configuration", slug: "configuration" },
        { label: "Language servers", slug: "language-servers" },
        { label: "Health check", slug: "health-check" },
        { label: "Reset", slug: "reset" },
        { label: "jj colocated setup", slug: "jj-colocated-setup" },
        { label: "Compared to other tools", slug: "compared-to-other-tools" },
        { label: "Development", slug: "development" },
      ],
    }),
  ],
});
