# Nautilus Logo Assets

The README pairs the [holographic comic mascot](nautilus-mascot.png) with a
[Nautilus wordmark](nautilus-wordmark.png). Bands of cyan, pink and gold
catch the ribs of its spiral shell. Each chamber resembles a cargo container,
a nod to the isolated workloads Nautilus runs. Use the transparent PNG for the illustrated mark
and the compact SVG marks below for smaller sizes.

## Files

| File | Use | Size |
|---|---|---|
| `nautilus-mascot.png` | Illustrated project logo for the README and docs | 1254 × 1254 PNG |
| `nautilus-wordmark.png` | Transparent project name, used beside the mascot or on its own | 1086 × 362 PNG |
| `logo.svg` | Compact shell mark for light backgrounds | any (vector) |
| `logo-dark.svg` | Dark backgrounds (salmon `#FF8A73`, septa `#1a1412`) | any (vector) |
| `logo-icon.svg` | Banner/medium icon (same mark, 2.5 turns) | any (vector) |
| `icon-32.svg` | 32px cut: 2 turns, 3 bold septa (septa dropped below this) | ≤32px |
| `icon-16.svg` | 16px cut: 1.5-turn spiral, no septa | ≤16px |
| `preview.html` | Size/contrast test sheet (open in browser) | — |

## Mascot generation

The previous mascot illustration was restyled with ImageGen using this prompt.

<details>
<summary>Full prompt</summary>

> Restyle the attached Nautilus project mascot as a premium holographic manga sticker for a GitHub README. Keep its left-facing chambered nautilus silhouette, curious expressive eye, curled tentacles, and unmistakable spiral shell made of ribbed shipping-container chambers. Redraw it with sweeping, tapered midnight-indigo comic inks, a lively upward tilt, a turquoise face, and a confident little smile. Make the shell the focal point: polished holographic foil with broad curved bands of electric cyan, violet and hot pink, a few warm amber flashes and crisp pearly glints flowing across the ribs. Use bold cel-shadow shapes and a few restrained halftone patches in the shadows. Keep the spiral and face clear at 160 px. Clean contour with a narrow cool-pearl rim for both light and dark backgrounds. One centered mascot, square composition, small clear margin, transparent background. No words, letters, frame, scenery, floating particles or diffuse bloom.

</details>

## Wordmark generation

The wordmark was generated with ImageGen and resized for the README.

<details>
<summary>Wordmark prompt</summary>

> A standalone horizontal typographic logo for the open-source project Nautilus. Render precisely the single word “Nautilus” in title case, all eight letters in that order, capital N and lowercase autilus, with a clearly dotted i. Lettering is the entire image. Create custom, confident slightly forward-leaning comic display lettering with a large open-counter lowercase a, a flowing rhythm and a distinctive tapered cut at the terminals. The broad strokes carry a coherent holographic foil ribbon of luminous electric cyan, lavender, hot magenta and a few amber reflections. Deliberate dark-indigo ink outlines and a slender pearl highlight at the outer edge, crisp cel-shaded depth and highly legible letter shapes. This wordmark pairs with a cartoon nautilus with an iridescent cargo-chamber shell, but draw only the lettering here: no animal, no standalone icon, no detached shapes or tagline. Keep the overall silhouette compact and horizontal with an approximately 4:1 width-to-height ratio, a nearly straight baseline, all letters and outlines fully visible with a modest safety margin. Transparent canvas with genuine alpha, no background, mockup, aura or cast shadow. Legible as a 260px-wide GitHub README wordmark on white and midnight backgrounds.

</details>

## Compact shell design

- **Concept**: nautilus shell cross-section — logarithmic spiral (radius ×2.2
  per full turn, matching real whorl proportions; the pure φ ratio reads as a
  comma, not a shell) + chamber septa crossing the full wall band on every
  visible whorl. Chambers = containers, spiral = runtime.
- **Anatomy**: solid wall band (thickness 0.40×radius, so outer whorls are fat
  and inner thin) with a visible white spiral channel; septa drawn per full
  turn (~4/whorl), almost full-span, slightly convex toward the core; the
  innermost whorl is left solid (clean center dot = protoconch); the outermost
  whorl ends in a wide body chamber with a flat aperture cut.
- **Color**: Rust orange `#CE422B` (light bg) / salmon `#FF8A73` (dark bg,
  with `#1a1412` septa).
- **Regenerate**: `docs/assets/gen_logo.py` reproduces all SVGs from
  parameters.

## Compact mark on light and dark backgrounds

```markdown
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.svg">
  <img src="docs/assets/logo.svg" width="120" alt="Nautilus">
</picture>
```

## AI image-generation prompt (for a polished/3D variant, optional)

> Minimalist logo of a nautilus shell cross-section, logarithmic spiral with
> curved chamber septa crossing the wall band, geometric line art, bold flat
> silhouette, Rust orange (#CE422B) on white background, flat vector style,
> centered composition, no text, no gradient mesh, generous negative space,
> tech open-source project aesthetic, clean edges, suitable for favicon
> scaling — negative prompt: photo, 3D render, texture, shadow, handwritten,
> organic noise, busy background.

## Known limits

- 16px reads as a generic curl/spiral (acceptable; accept the aperture notch
  as the 16px identity cue). 32px is the practical floor for the septa.
- Innermost whorl spacing tightens as designed; if a v1.1 pass wants more
  air, nudge the center-dot floor radius up a touch.
