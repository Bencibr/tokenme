# Cream ginger kitten

`kitten-sprites.png` is an original AI-generated transparent raster atlas,
created with the built-in image generation tool for TokenMe. It has four
poses in reading order: open eyes, half blink, closed blink, happy wave.
The source image is 1254 x 1254 pixels with alpha, and is kept unchanged.

`bubble.css` aligns the head center and paw baseline for each pose before
playing the blink sequence. Breathing, waving and carried motion animate
the page layer; they never recreate, resize or hide the native window.

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

```text
Image 1 is the EDIT TARGET. Correct this kitten specifically for peeking DOWN from the TOP border of a desktop. Current front-facing upright flat portrait is WRONG. Change the perspective: head tips and bows DOWN into the screen, looking down toward the viewer's mouse BELOW the top border. Show more of the top of the forehead/head, chin tucked slightly back, ears farther up and back, muzzle angled downward, lower eye placement / slightly foreshortened eyes consistent with the downward head tilt. The kitten is above the desktop, forepaws cling to the horizontal TOP edge, head leans OVER it looking DOWN. Both forepaws remain at the top left and top right, higher than the head. Body stays mostly out of view. This must visually read as a kitten leaning over a high ledge looking down, NOT a front portrait with paws raised. No upside-down whole-image flip. Keep the same cream-and-ginger kitten, fur markings, cute friendly face and soft illustrated style. Keep white eye sockets with NO iris/pupils so a separate live pupil layer can be placed toward their lower part. Square PNG with genuine alpha transparency; all ears/paws/head fully visible within padding. DO NOT draw any actual edge, ledge, monitor, horizontal bar, border, background, text, props or scenery.
```
