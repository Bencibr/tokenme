# Cream ginger kitten

`kitten-sprites.png` is an original AI-generated transparent raster atlas,
created with the built-in image generation tool for TokenMe. It has four
poses in reading order: open eyes, half blink, closed blink, happy wave.
The source image is 1254 x 1254 pixels with alpha, and is kept unchanged.

`pet-skins.json` supplies each frame's alignment and eye anchors. Breathing,
waving and carried motion animate the page layer. The native window adapts to
the selected skin's layout box; it stays at that size throughout a blink.

`kitten-edge-side.png`, `kitten-edge-left.png` and `kitten-edge-top.png`
are dedicated right-edge, left-edge and top-hanging poses. Their empty eye
sockets are filled by separately rendered pupils that follow the pointer.
Each pose has its own calibrated eye anchors. The top pose's paws grip the
top edge while its head hangs into the desktop. No frame/bar is drawn into
any image.

Generation prompt:

### tokenmePeekSide

```text
Use case: stylized-concept. Asset: transparent PNG illustration for an interactive tiny Windows desktop pet. Image 1 is a CHARACTER IDENTITY reference, not an edit target. Keep precisely the same cute cream-and-ginger kitten's face, triangular ears, cream muzzle, ginger markings, rosy cheeks and soft dark-brown outline. Actual alpha transparency, no background, no physical monitor/frame/border/ledge, no ground/shadow scene, no text or accessories. Front-facing head with large outlined EMPTY WHITE eye sockets: draw the white sclera and eyelid outlines but NO brown iris, NO pupil and NO eye highlights, because the app overlays tracked pupils. Eyes symmetric, perfectly level. Full head and both paws entirely visible, clean silhouette and no halo. Polished warm soft cartoon matching reference. Square canvas with roomy alpha padding. Pose: kitten lying horizontally and peeking around an imaginary VERTICAL edge at the RIGHT side. Head is on the left, cute tilted-small body tucked to the right behind the imaginary edge, two little front paws grip the right-hand edge with visible pink toe beans. Face stays FRONT-facing and upright, eye sockets stay level (not profile). Paw grip points vertically stacked on the right of the face. Keep kitten head at about 45% canvas x, 45% canvas y, width 54% canvas; total character occupies about 78% canvas. Do not draw the edge itself.
```

### tokenmePeekLeft

```text
Use case: stylized-concept. Asset: transparent PNG desktop pet edge pose. Image 1 is character identity reference. Generate the SAME adorable cream-and-ginger kitten with the same facial features, fur markings, large round head, triangular ears, rosy cheeks, tiny pink nose and soft brown outline. Pose: kitten lying horizontally and peeking around an imaginary VERTICAL edge at the LEFT side. Its tucked little body is on the left, head on the right looking into the screen, two little forepaws with pink toe beans gripping the LEFT edge with one paw above the other. Head FRONT-facing, upright, not profile. Two symmetrical, level, large empty white eye sockets with dark eyelid outlines; NO iris, NO pupil, NO highlights (the app adds cursor-following pupils). Both paws and full head completely visible. Square canvas, head center approximately 55 percent canvas x and 45 percent canvas y, head width about 54 percent canvas, whole kitten about78percent canvas, roomy transparent padding. Actual alpha transparency everywhere outside character. No actual window, edge, frame, border, ledge, line, backdrop, checkerboard, text, numbers, props, accessories or ground shadow. Soft polished cartoon sprite matching reference.
```

### tokenmePeekTop

```text
Image 1 is the EDIT TARGET. Keep this same kitten's head, ears, fur colors, facial features, empty white eye sockets and transparent background. Change ONLY the paw placement / pose: remove the two paws currently located beside the cheeks and chin. Put both forepaws at the VERY TOP of the composition above the ears, gripping an imaginary HORIZONTAL TOP screen edge. The head hangs upright BELOW the pair of paws, as a kitten dangling and peeking DOWN into the desktop from its top border. Paws visible near the upper-left and upper-right corners, pink toe beans and small curled fingers, a short connecting arm allowed. No paws at cheek or chin height. Keep the face upright, same scale and same location as much as possible, eyes EMPTY WHITE with no iris/pupils so the app can overlay them. Actual alpha transparency, no real edge, bar, border, background, ledge, lettering, shadow scene or new props. This is the top-edge hanging pose, NOT a kitten peeking up from a bottom edge.
```

```text
Use case: stylized-concept.
Asset type: production-ready transparent PNG sprite atlas for a tiny Windows desktop pet, not a mockup.
Create exactly ONE square sprite sheet containing exactly four full-body poses of the SAME adorable cream-and-ginger kitten, arranged on an EXACT 2 by 2 grid of equal square cells. Output canvas square. Each kitten stays centered in its own cell with identical scale, anatomy, head position, sitting body, tail direction and feet baseline, leaving clear empty alpha padding between cells. No visible grid, cell borders, labels, captions, checkerboard, floor, background, props, or cast ground shadows. Actual alpha transparency everywhere outside the kitten.
Style: polished cute soft 2D cartoon game sprite, rounded big head and small body, short triangular ears, pale cream face and chest, warm ginger patches and tail, little rosy cheeks, tiny pink nose, soft dark brown clean outlines, readable silhouette at 76 pixels. Front-facing, both ears/paws/tail entirely visible in all frames. No accessories.
Frames in reading order:
TOP LEFT: neutral friendly sitting pose, large shiny dark round eyes open, gentle tiny mouth.
TOP RIGHT: identical neutral pose, eyes halfway closing for a blink. Change ONLY eyelids, not body/head position.
BOTTOM LEFT: identical neutral pose, eyes fully closed gently for a blink. Change ONLY eyelids, not body/head position.
BOTTOM RIGHT: same kitten, very happy expression with closed smiling eyes, small smile and one little paw raised slightly to wave, keeping head/body/feet baseline the same.
Alignment matters more than variety: four equal tiles, same stable kitten in each tile. Characters occupy about 78 percent of cell height and fit fully inside each tile. Transparent edges clean, no white halos. All four kittens the same size. Do not combine tiles or add extra frames.
```

Prompt summary:

> Create a square transparent PNG sprite atlas with exactly four full-body
> poses of the same adorable cream-and-ginger kitten on a two-by-two grid.
> Rounded big head, small sitting body, short triangular ears, cream face
> and chest, ginger patches and tail, rosy cheeks, tiny pink nose, soft dark
> brown outlines, clear silhouette at small desktop size. Front-facing;
> both ears, paws and tail fully visible. Keep identical character, scale,
> head position and feet baseline across poses. In reading order: friendly
> open eyes; same pose with halfway closed eyelids; same pose with closed
> eyelids; happy smiling eyes and one paw raised to wave. Leave transparent
> padding. No grid lines, text, numbers, props, floor, checkerboard,
> watermark, background or cast ground shadows. Actual alpha transparency,
> clean edges, no white halos.

## Final top pose correction

The shipped top pose uses a bowed head looking down into the screen, with
paws above the head. Its first cursor sample replaces a downward default
gaze. Top eye anchors are independently calibrated for the foreshortened
eye sockets. The earlier upright pose is not the shipped asset.

## Top pose v2

`kitten-edge-top-v2.png` supersedes the first top pose. It keeps the same
identity but changes the silhouette to a compact kitten lying over the top
edge: the torso connects to both forepaws, the head leans forward, and the
face looks down into the desktop. The old `kitten-edge-top.png` remains in the
asset directory as a rollback reference and is no longer imported by the
renderer.

## Ancient-style character skins

The Windows bubble also ships two complete ancient-style cartoon character
skins. Each set has one four-frame free-state atlas plus dedicated right,
left, and top edge poses:

- `han-girl-sprites.png`, `han-girl-edge-side.png`,
  `han-girl-edge-left.png`, `han-girl-edge-top.png` — a cute girl in dusty
  rose and muted-jade hanfu with twin buns and jade ornaments.
- `han-boy-sprites.png`, `han-boy-edge-side.png`,
  `han-boy-edge-left.png`, `han-boy-edge-top.png` — a cute boy in ink-blue
  and teal hanfu with a topknot and jade crown clasp.

The free atlases use the same four-frame contract as the kitten (open,
half-blink, closed-blink, happy wave). Edge images leave the eye sockets
empty so the application can render pointer-following pupils. All nine new
rasters were generated with genuine alpha transparency and explicitly avoid
drawing a screen edge, bar, frame, background, or text into the asset.

## Modern girl skin

The Windows bubble also includes a modern-style girl skin with a lavender
hoodie, headphones, dark pleated skirt and short dark bob. Its four files are:

- `modern-girl-sprites.png` — the four-frame free-state atlas;
- `modern-girl-edge-side.png` — the right-edge pose;
- `modern-girl-edge-left.png` — the left-edge pose;
- `modern-girl-edge-top.png` — the downward-looking top-edge pose.

The atlas follows the same open / half-blink / closed-blink / happy-wave
contract. All four rasters use genuine alpha transparency and leave the eye
sockets empty for the live pointer-following pupil layer.

## Summer long-legged beauty

`summer_beauty` adds an adult modern summer character with a mint sleeveless
top, off-white tailored shorts, white sandals and a dark chestnut high ponytail.
The full-body illustration retains long legs at a 192px artwork size; the
window adapts to a 260px host instead of compressing it into the small pet box.
The four assets are `summer-beauty-sprites.png`, `summer-beauty-edge-side.png`,
`summer-beauty-edge-left.png` and `summer-beauty-edge-top.png`. The standing,
waving, left, right and top poses each have independent eye anchors. The
original transparent PNGs are preserved. The built-in generation prompt set
is recorded in [summer-beauty-prompts.md](summer-beauty-prompts.md).

## Pure summer beauty

`pure_summer_beauty` adds an adult Asian summer-fashion character with fair
skin, shoulder-length brown hair, a fitted white top, pale blue pleated short
skirt and white sneakers. The free pose keeps a small waist reveal for a more
fashion-forward silhouette while remaining a non-explicit desktop companion.
The four assets are `pure-summer-beauty-sprites.png`,
`pure-summer-beauty-edge-side.png`, `pure-summer-beauty-edge-left.png` and
`pure-summer-beauty-edge-top.png`. The top pose leans over the screen and
looks down; the side poses face inward. The generation prompt set is recorded
in [pure-summer-beauty-prompts.md](pure-summer-beauty-prompts.md).

## Archived model skin packages (GLB / VRM)

The 3D model path is currently archived as a TODO and is hidden from the
Windows skin selector. Existing model files and the renderer remain in this
directory for a later, deliberate redesign; an older saved model selection is
automatically shown as the classic waterdrop instead of opening the unfinished
path. Do not add new model entries to the user-facing list until the package
contract has explicit materials, parts, actions, head/eye tracking, and a
reliable camera-fit validation.

## Model skin packages (GLB / VRM)

The 3D path is intentionally split into an offline generation step and a small
Windows runtime. A generated model is cleaned, reduced and rigged outside the
installer, then exported as `.glb` or `.vrm`. TokenMe only loads that local
asset with Three.js; it does not ship Python, CUDA or an image-to-3D model.

Put the model below `src/assets/pets/models/` and add a `render: "model"`
entry to `pet-skins.json`. The build gate checks the model path before Vite
bundles it. A VRM model uses its own humanoid and look-at metadata for head and
eye tracking; a plain GLB can name a custom head node in `model.head.bone`.

The shipped `seed_san_3d` package embeds the official Seed-san VRM sample from
VirtualCast, Inc. Its model metadata permits redistribution and modification
redistribution under VRM Public License 1.0; the matching attribution notice is
kept beside the model in `models/seed-san.LICENSE.txt`. Seed-san follows the
VRM 1.0 sample convention and has no GLTF animation clips, so its manifest
enables the runtime idle loop: breathing/bobbing, gentle sway, arm and head
motion, VRM blink expressions, spring-bone hair motion and pointer-following
look-at. A model package therefore remains animated even when the source file
contains no baked animation.

`animated_human_3d` currently embeds a CC0 Maya GLB proof asset from
Innerscene/MakeHuman. It is retained for future engineering work only; it is
not a final cute character skin and is not exposed in the selector.

Example:

```json
{
  "portrait_vrm": {
    "labelKey": "set.bubble.skin.portrait_vrm",
    "render": "model",
    "model": {
      "file": "models/portrait.vrm",
      "format": "vrm",
      "scale": 1,
      "camera": { "fov": 24, "height": 0.5, "targetY": 0.5 },
      "head": { "enabled": true, "maxYaw": 24, "maxPitch": 16 },
      "poses": {
        "right": { "cameraYaw": -8, "anchor": [0.08, 0] },
        "left": { "cameraYaw": 8, "anchor": [-0.08, 0] },
        "top": { "cameraPitch": -8, "anchor": [0, -0.04] }
      }
    },
    "layout": {
      "content": 224,
      "dockShift": { "right": -72, "left": 72, "top": 66 }
    }
  }
}
```

The model generator is not part of the desktop app. For human reconstruction,
ECON is useful for a non-commercial research prototype because it can produce
clothed humans and SMPL-X animation; TRELLIS or Hunyuan3D are better treated as
separate offline image-to-3D services, followed by retopology and rigging. The
runtime dependency is `three` plus `@pixiv/three-vrm`; keep the resulting model
small enough for a transparent always-on-top WebView window.

## Skin package contract

`pet-skins.json` is the registry for raster and model pets. Raster packages are
rendered by `PetSkin`; model packages are rendered by `PetModel` through the
same window-size and docking contract. The renderer does not contain
per-skin coordinates in CSS: it reads each package's asset names, layout, atlas
frame offsets, edge shifts, eye anchors and eyelid colours, then exposes them as
CSS variables. The four eye maps are `none`, `right`, `left` and `top`; points
are measured in the package's rendered `art` box, from its top-left corner.
`eyes.happy` overrides the eye anchors for the free-state waving frame. Set
`enabled: false` when a pose already paints pupils or smiling closed eyes;
otherwise the renderer adds only the tracked pupils and eyelids, preserving
the PNG's eye outline. Optional `rotation: [leftDegrees, rightDegrees]` matches
tilted eye sockets. Eye positions include that frame's alignment offset.
Use `tracking: "native"` for realistic raster faces whose original eye shape
must remain untouched; this keeps the blink eyelids but does not draw the
cartoon pupil layer. `tracking: "pupil"` is the default for illustrated skins.
Pupil travel is bounded by the eye's size. Optional `eyelid.strokeWidth` sets
the closed-eye line width for skins with small, adult-proportioned faces.

Use `animation.blink: "overlay"` for empty-eye atlases: only the eyelids blink,
so independently generated character frames cannot make the body jump. Use
`"atlas"` for aligned atlases with baked eyes, with `eyes.none.enabled: false`.
During a skin switch the whole pet, including its eyes and token badge, stays
hidden until all four images finish decoding. Completions from replaced skins
are ignored; the layout box remains measurable while the images load.
The renderer clips aligned frames to their original tile, preventing a nearby
tile's shoes or hair from leaking into the chosen pose. Size and motion always
come from the same package, including after switching skins while docked.

`pnpm typecheck` and `pnpm build` run `scripts/check-pet-skins.mjs` first. The
gate checks referenced PNGs, dimensions, frame offsets, eye geometry and label
translations. Run `node scripts/check-pet-skins.mjs --self-test` from the repo
root to check the rejection paths for malformed packages.

To add a skin, add its four transparent PNGs, add one entry under
`pet-skins.json -> skins`, and add the two translation strings referenced by
`labelKey`. The atlas must be a 2x2 sheet in open / half-blink / closed-blink /
happy order. Edge images are separate because their silhouettes and eye lines
change with the screen edge. No CSS or renderer change is needed for a new
package; the Windows settings value stores the package ID as an opaque string.

The entry shape is intentionally small and explicit:

```json
{
  "labelKey": "set.bubble.skin.example",
  "assets": { "atlas": "example-sprites.png", "side": "example-edge-side.png", "left": "example-edge-left.png", "top": "example-edge-top.png" },
  "layout": { "content": 112, "art": 112, "atlas": 224, "artOffset": 0, "dockShift": { "right": -35, "left": 35, "top": 34 } },
  "frames": { "open": [0, 0], "half": [-112, 0], "closed": [0, -112], "happy": [-112, -112] },
  "animation": { "blink": "overlay" },
  "eyes": {
    "none": { "left": [60, 40], "right": [75, 40], "size": 14, "height": 15, "pupil": 8 },
    "happy": { "left": [39, 38], "right": [55, 36], "size": 14, "height": 15, "pupil": 8 },
    "right": { "left": [66, 40], "right": [83, 34], "size": 12, "height": 14, "pupil": 8 },
    "left": { "left": [26, 45], "right": [42, 41], "size": 12, "height": 14, "pupil": 8 },
    "top": { "left": [44, 65], "right": [66, 57], "size": 13, "height": 14, "pupil": 8 }
  },
  "eyelid": { "fill": "#f5dfef", "stroke": "#493449" }
}
```

```text
Image 1 is the EDIT TARGET. Correct this kitten specifically for peeking DOWN from the TOP border of a desktop. Current front-facing upright flat portrait is WRONG. Change the perspective: head tips and bows DOWN into the screen, looking down toward the viewer's mouse BELOW the top border. Show more of the top of the forehead/head, chin tucked slightly back, ears farther up and back, muzzle angled downward, lower eye placement / slightly foreshortened eyes consistent with the downward head tilt. The kitten is above the desktop, forepaws cling to the horizontal TOP edge, head leans OVER it looking DOWN. Both forepaws remain at the top left and top right, higher than the head. Body stays mostly out of view. This must visually read as a kitten leaning over a high ledge looking down, NOT a front portrait with paws raised. No upside-down whole-image flip. Keep the same cream-and-ginger kitten, fur markings, cute friendly face and soft illustrated style. Keep white eye sockets with NO iris/pupils so a separate live pupil layer can be placed toward their lower part. Square PNG with genuine alpha transparency; all ears/paws/head fully visible within padding. DO NOT draw any actual edge, ledge, monitor, horizontal bar, border, background, text, props or scenery.
```
