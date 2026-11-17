# Subhost brand mark

Design read: identity for a single-node ledger written in Rust, for engineers who read
the implementation table before the headline, in a technical-documentation style with a
printmaker's accent. Dial: ENERGY 2 / RHYTHM 1 / MOTION 1. A mark is static by nature,
so motion stays at zero everywhere it is used.

## The idea

Two elements, nothing else.

A graphite square rule is the host: one machine, one disk, one boundary. Inside it sits a
single seal-red block, seated in the lower-left corner and embedded in the rail it touches.
The block is not floating in the middle of the frame. It is committed into the boundary,
which is what this code does: it writes to the ledger before it updates state.

Lower-left is the "sub" position. It is below the reading baseline and first in reading
order, so the name and the shape agree without drawing a letter S.

There is no network of nodes, no orbit, no cube. The repository is one node and says so
in its own README, so the mark shows one block and one boundary.

## Construction

- Grid 96 x 96. Frame at 8..88, stroke 10, square miters, no corner radius anywhere.
- Frame opening: 13..83, so 70 units square.
- Block 26 x 26 at (13, 57). It meets the inner face of the left rail and the inner face of
  the bottom rail, and leaves 44 units of open space above and to its right. The block
  belongs to the corner; it cannot be read as a centered element.
- Favicon variant uses a 12 stroke and a 30 block. Optical compensation, not a different
  design: at 16 px the standard stroke thins into a grey hairline.
- No gradient, no shadow, no glow, no outline, no rotation. The mark is two shapes.

## Palette

| Token | Hex | RGB | HSL | Use |
|-------|-----|-----|-----|-----|
| Graphite | `#14181C` | 20, 24, 28 | 210, 17%, 9% | Frame, wordmark, body text |
| Paper | `#F3EFE7` | 243, 239, 231 | 40, 33%, 93% | Light ground, reversed ink |
| Seal | `#C8452B` | 200, 69, 43 | 10, 65%, 48% | The single accent block |
| Seal bright | `#E4603F` | 228, 96, 63 | 12, 75%, 57% | The same block on dark ground |

Contrast: seal on paper 4.2:1, seal bright on graphite 5.1:1, graphite on paper 15.6:1.

Seal red is a mark color, not a text color. Body copy uses graphite on paper, or paper on
graphite. The palette is two neutrals plus one accent, and the accent appears exactly once
per composition.

## Typography

IBM Plex Sans (SIL Open Font License 1.1). `sub` at weight 500, `host` at weight 700,
lowercase only, tracking -0.014 em. The weight split states the relationship: the container
is light, the host is heavy.

Plex was chosen because it was drawn for technical documentation and keeps small humanist
details in its terminals, so the wordmark does not read as another geometric startup mark.
Inter and the usual monospace faces were rejected as defaults. A monospace wordmark would
be the obvious dev-tool move, and this is a wordmark, not a terminal prompt.

The wordmark is outlined in every SVG. No file depends on a font being installed. If you
need to set new text in the brand face, install Plex rather than substituting another family.

## Clear space and minimum size

- Clear space equals the width of the seal block (26/96 of the mark height) on all four sides.
- Mark: 24 px minimum. Below 24 px use `favicon.svg`, which carries the heavier stroke.
- Horizontal lockup: 96 px wide minimum. Below that use the mark alone.
- The accent block never scales independently of the frame.

## Misuse

- Do not recolor the block. It is seal red or, in one-color reproduction, graphite.
- Do not add a gradient, drop shadow, glow, outline, bevel, or corner radius.
- Do not rotate, stretch, condense, or perspective the mark.
- Do not move the block to the center, the top, or the right. The lower-left seat is the idea.
- Do not set the wordmark in another typeface, or re-typeset it with different tracking.
- Do not place the lockup on a photograph or a patterned ground. Use the mark on a solid
  paper or graphite field with the clear space above.
- Do not reverse the lockup on mid-tone grey. Use graphite or paper ground only.

## Files

| File | What it is |
|------|------------|
| `subhost-logo.svg` | Primary horizontal lockup, graphite on transparent |
| `subhost-logo-on-dark.svg` | Lockup for graphite or dark grounds |
| `subhost-logo-mono.svg` | One-color lockup, all black, recolor as a single fill |
| `subhost-mark.svg` | Mark alone, graphite and seal |
| `subhost-mark-on-dark.svg` | Mark alone for dark grounds |
| `subhost-mark-mono.svg` | Mark alone, one color |
| `subhost-wordmark.svg` | Wordmark alone, outlined |
| `favicon.svg` | Mark with optical compensation for 16 to 24 px |
| `favicon.ico` | 16, 32 and 48 px icon |
| `png/subhost-mark-{512,256,128,64,32,16}.png` | Transparent PNG exports |
| `png/subhost-logo-{1200,600}w.png`, `png/subhost-logo-1200w-on-dark.png` | Lockup exports |
| `png/favicon-{16,32,48}.png` | Icon source sizes |

`assets/brand/subhost-logo.svg` is the primary lockup, used by the README header. The
brand directory is the only source; there is no second copy under `assets/`.

## Why each decision

- **Square frame:** the host is a boundary, and a boundary is a rule, so the rule has no radius.
- **One block, lower left:** the product is one node and it sits below, not at the center.
- **Seal red:** a stamped seal is the older, clearer signal of "committed" than a checkmark,
  and it keeps the mark out of the blue-gradient field every chain logo already occupies.
- **Warm paper, not white:** a ledger is paper before it is a screen, and warm paper makes
  the graphite look chosen instead of default.
- **Plex 500 / 700:** weight, not color, carries the sub / host relationship, so the mark
  survives one-color printing.
- **No gradient or shadow:** the rest of the repository is measured and undecorated; a lit
  logo would be the only thing in it that is trying to look bigger than it is.

## Replaced mark

The previous `assets/logo.svg` was a blue-to-indigo gradient on a dark rounded square with a
drop shadow, two diagonal plates, and four decorative grid ticks. It carried no property of
this repository and would have fitted any chain project. The file is deleted; the lockup in
this directory replaces it, and the README header now points here.