# Sustain landing page

A single static page that the outreach campaign (see issue #75) points at.
Zero JavaScript, zero build step, light/dark aware, self-contained in this
directory.

```
website/
├── index.html      # the page
├── style.css       # all styling (light + dark via prefers-color-scheme)
└── assets/         # logo, icon, screenshots
```

## Screenshots

Screenshots live in `assets/` and are copied from the repository's
`.github/assets/` (which the project README also uses). If you update or add
one there, copy it here so the page stays self-contained:

```sh
cp ../.github/assets/<name>.png assets/
```

then reference it from `index.html` as `assets/<name>.png`.

## Deployment

The site is published to GitHub Pages by `.github/workflows/pages.yml`, which
uploads this directory whenever anything under `website/` changes. Pages is
configured with the **GitHub Actions** source (Settings → Pages → Source).
Live at <https://open-sustain.github.io/sustain/>.

For a custom domain, set it in Settings → Pages and add a `CNAME` file here
containing the domain so it survives redeploys. The page uses relative asset
paths, so it works at either the project subpath or a domain root unchanged.

## A note on analytics

The page deliberately ships **no** tracking script, because it promises "no
tracking" to a justifiably suspicious Linux audience. If you want anonymous,
cookieless visit counts for the campaign, add a self-hosted GoatCounter or
Plausible snippet to `index.html`'s `<head>` **and** tighten the on-page
wording to say exactly what it counts. Never add anything that sets cookies,
fingerprints visitors, or reports to an ad network.
