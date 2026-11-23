# Nautilus Logo Assets

The README features [the full-color mascot](nautilus-mascot.png). Its spiral
shell has ribbed chambers shaped like cargo containers, a nod to the isolated
workloads Nautilus runs. Use the transparent PNG for the illustrated mark
and the compact SVG marks below for smaller sizes.

## Files

| File | Use | Size |
|---|---|---|
| `nautilus-mascot.png` | Illustrated project logo for the README and docs | 1254 × 1254 PNG |
| `logo.svg` | Compact shell mark for light backgrounds | any (vector) |
| `logo-dark.svg` | Dark backgrounds (salmon `#FF8A73`, septa `#1a1412`) | any (vector) |
| `logo-icon.svg` | Banner/medium icon (same mark, 2.5 turns) | any (vector) |
| `icon-32.svg` | 32px cut: 2 turns, 3 bold septa (septa dropped below this) | ≤32px |
| `icon-16.svg` | 16px cut: 1.5-turn spiral, no septa | ≤16px |
| `preview.html` | Size/contrast test sheet (open in browser) | — |

## Mascot generation

The illustrated mark was generated with ImageGen using this prompt.

<details>
<summary>Full prompt</summary>

> Logo-brand. Design one polished original mascot mark for Nautilus, a Rust implementation of the Kubernetes Container Runtime Interface that runs isolated OCI containers and Wasm workloads. A lively little chambered nautilus cephalopod in side profile, expressive eye and a few short curling tentacles, carrying an oversized coiled shell. Build the spiral from distinct colorful chambers following its curvature; a few outer chambers resemble tiny stackable cargo containers through simple broad ribs. Make the organic shell and the container idea read as one shape. Rust-orange and coral shell, turquoise body, sunny yellow and indigo accent chambers, deep ink outlines, minimal flat cel shading. A spirited illustrated open-source mascot, crafted silhouette legible at 96 px. Square composition, centered with small breathing room, entire mark visible, genuine transparent background. No scenery, lettering, labels, text, official tech logos or extra emblems.

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
