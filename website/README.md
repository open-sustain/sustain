# Sustain landing page

A single static page that the Google Ads campaign points at (see issue about
outreach). Zero JavaScript, zero build step, light/dark aware, self-contained
in this directory.

```
website/
├── index.html      # the page
├── style.css       # all styling (light + dark via prefers-color-scheme)
└── assets/         # logo, icon, screenshots
```

## Adding the upcoming screenshots

`index.html` has two clearly-marked placeholder figures. To fill them:

1. Drop the image into `assets/`:
   - Smart Shuffle → `assets/screenshot-smart-shuffle.png`
   - Device sync → `assets/screenshot-device-sync.png`
2. Replace the matching `<figure class="shot shot--placeholder">` block with a
   real one (the HTML comment right above each placeholder shows the exact
   markup to paste).

The other screenshots are copied from `.github/assets/`; if you update those,
re-copy them here so the page stays self-contained.

## Deploying to GitHub Pages

Two equally-fine options:

- **Pages → "Deploy from a branch", folder `/website`** (simplest): enable it in
  the repo settings, no workflow needed.
- **Pages → GitHub Actions**: upload this directory as the Pages artifact from a
  workflow. Use this if you later add a build step.

Set up a custom domain in the same settings screen if you want one; add a
`CNAME` file here with the domain.

## A note on analytics

The page deliberately ships **no** tracking script, because it promises "no
tracking" to a justifiably suspicious Linux audience. If you want anonymous,
cookieless visit counts for the campaign, add a self-hosted GoatCounter or
Plausible snippet at the marked spot in `index.html`'s `<head>` **and** tighten
the on-page wording to say exactly what it counts. Never add anything that sets
cookies, fingerprints visitors, or reports to an ad network.
