# Sustain landing page

A single static page that the outreach campaign (see issue #75) points at.
Zero JavaScript, zero build step, light/dark aware, self-contained in this
directory.

```
website/
├── index.html      # the page
├── style.css       # all styling (light + dark via prefers-color-scheme)
└── assets/         # logo, icon, screenshots (each .png paired with a .webp)
```

## Screenshots

Screenshots live in `assets/` and originate from the repository's
`.github/assets/` (which the project README also uses). The page serves a
lighter **WebP** to browsers that accept it and falls back to the **PNG**
everywhere else, via `<picture>`:

```html
<picture>
  <source srcset="assets/<name>.webp" type="image/webp" />
  <img src="assets/<name>.png" alt="…" />
</picture>
```

So every on-page image ships as both files. The PNG is the source of truth
(copied from `.github/assets/`); the WebP is generated from it. To add or
refresh one:

```sh
cp ../.github/assets/<name>.png assets/

# Generate the WebP: encode both lossless and lossy, keep whichever is
# smaller. Text-dense UI shots (the Songs table, dialogs, settings panels)
# usually win lossless and stay pixel-perfect; photographic shots (cover-art
# grid, charts) win lossy with no visible loss. Bump -quality to 93 for the
# large, prominent hero/gallery shots.
magick assets/<name>.png -define webp:lossless=true -define webp:method=6 /tmp/ll.webp
magick assets/<name>.png -quality 90 -define webp:method=6              /tmp/lossy.webp
cp "$([ "$(stat -c%s /tmp/ll.webp)" -le "$(stat -c%s /tmp/lossy.webp)" ] \
      && echo /tmp/ll.webp || echo /tmp/lossy.webp)" assets/<name>.webp
```

then reference it from `index.html` with the `<picture>` block above. The
favicon (`<link rel="icon">`) and the `og:image` social preview stay PNG —
`<picture>` doesn't apply to them and PNG is the no-surprises choice there.

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
