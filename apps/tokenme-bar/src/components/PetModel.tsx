import { useEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import * as THREE from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { VRMLoaderPlugin, VRMUtils, type VRM } from "@pixiv/three-vrm";
import type { PetDock, ModelPetSkinConfig, PetSkinName } from "../lib/petSkinRegistry";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";

type Gaze = { x: number; y: number } | null;
type ModelDock = Exclude<PetDock, "none">;
type ModelState = { dock: ModelDock | null; hovered: boolean; dragging: boolean; gaze: Gaze };
type ActionMode = 0 | 1 | 2 | 3;
type ModelActionMap = { idle: number; wave: number | null; jump: number | null; walk: number | null };

type PetModelProps = {
  skin: PetSkinName;
  config: ModelPetSkinConfig;
  dock: "left" | "right" | "top" | null;
  hovered: boolean;
  dragging: boolean;
  tokens: number;
  gaze: Gaze;
  onAssetError: () => void;
};

type ModelRuntime = {
  root: THREE.Object3D;
  vrm?: VRM;
  mixer?: THREE.AnimationMixer;
  actions: THREE.AnimationAction[];
  actionNames: string[];
  actionMap: ModelActionMap;
  activeAction: number;
  activeLoop: boolean;
  motionBones: MotionBone[];
  height: number;
  width: number;
  depth: number;
  baseRotation: THREE.Euler;
  basePosition: THREE.Vector3;
};

type MotionBone = {
  node: THREE.Object3D;
  role: "chest" | "leftArm" | "rightArm" | "head";
  base: THREE.Quaternion;
  amplitude: number;
  phase: number;
};

type ModelInteraction = {
  pointerId: number | null;
  x: number;
  y: number;
  moved: boolean;
  yaw: number;
  pitch: number;
  hovering: boolean;
  hoverYaw: number;
  hoverPitch: number;
  actionMode: ActionMode;
  actionStartedAt: number;
};

type ModelStyle = CSSProperties & Record<`--${string}`, string>;

const clamp = (value: number, min: number, max: number) => Math.max(min, Math.min(max, value));
const degrees = (value: number) => value * Math.PI / 180;
const actionDurations: Record<ActionMode, number> = { 0: 0, 1: 1900, 2: 1700, 3: 2300 };

function updatePointerLook(current: ModelInteraction, event: React.PointerEvent<HTMLCanvasElement>) {
  const bounds = event.currentTarget.getBoundingClientRect();
  if (!bounds.width || !bounds.height) return;
  const x = clamp((event.clientX - bounds.left) / bounds.width * 2 - 1, -1, 1);
  const y = clamp((event.clientY - bounds.top) / bounds.height * 2 - 1, -1, 1);
  current.hovering = true;
  current.hoverYaw = x * 34;
  current.hoverPitch = -y * 22;
}

function disposeModel(root: THREE.Object3D) {
  // VRMUtils also handles textures and material arrays used by MToon. It is
  // safe for ordinary GLB scenes, which is important because the manifest
  // accepts both formats.
  VRMUtils.deepDispose(root);
}

function normalizeModel(root: THREE.Object3D, config: ModelPetSkinConfig): { height: number; width: number; depth: number; rotation: THREE.Euler; position: THREE.Vector3 } {
  const model = config.model;
  const isVrm = model.format === "vrm" || model.file.toLowerCase().endsWith(".vrm");
  const rotationY = model.rotationY ?? (isVrm ? Math.PI : 0);
  root.rotation.y = rotationY;
  root.scale.setScalar(model.scale ?? 1);
  root.updateMatrixWorld(true);

  const bounds = new THREE.Box3().setFromObject(root);
  const center = bounds.getCenter(new THREE.Vector3());
  const height = Math.max(bounds.max.y - bounds.min.y, 0.1);
  const width = Math.max(bounds.max.x - bounds.min.x, 0.1);
  const depth = Math.max(bounds.max.z - bounds.min.z, 0.1);
  // Put the feet on a stable local floor and center the character before the
  // camera is fitted. This keeps different generated humans interchangeable.
  root.position.x -= center.x;
  root.position.y -= bounds.min.y;
  root.position.z -= center.z;
  return { height, width, depth, rotation: root.rotation.clone(), position: root.position.clone() };
}

function findBone(root: THREE.Object3D, names: string[]): THREE.Object3D | null {
  const wanted = names.map((name) => name.replace(/[\s_-]/g, "").toLowerCase());
  let result: THREE.Object3D | null = null;
  root.traverse((node) => {
    if (result || node.type !== "Bone") return;
    const name = node.name.replace(/[\s_-]/g, "").toLowerCase();
    if (wanted.some((part) => name === part || name.includes(part))) result = node;
  });
  return result;
}

function createMotionBones(root: THREE.Object3D, vrm?: VRM): MotionBone[] {
  const bones: MotionBone[] = [];
  const add = (role: MotionBone["role"], node: THREE.Object3D | null, amplitude: number, phase: number) => {
    if (!node || bones.some((entry) => entry.node === node)) return;
    bones.push({ role, node, base: node.quaternion.clone(), amplitude, phase });
  };
  const humanoid = vrm?.humanoid;
  add("chest", humanoid?.getNormalizedBoneNode("chest") ?? humanoid?.getNormalizedBoneNode("upperChest") ?? findBone(root, ["upperChest", "chest", "spine2", "spine"]), 0.028, 0);
  add("leftArm", humanoid?.getNormalizedBoneNode("leftUpperArm") ?? findBone(root, ["leftUpperArm", "leftArm", "lArm"]), 0.065, Math.PI);
  add("rightArm", humanoid?.getNormalizedBoneNode("rightUpperArm") ?? findBone(root, ["rightUpperArm", "rightArm", "rArm"]), 0.065, 0);
  add("head", humanoid?.getNormalizedBoneNode("head") ?? findBone(root, ["head"]), 0.035, 0.6);
  return bones;
}

function applyProceduralMotion(runtime: ModelRuntime, elapsed: number, config: ModelPetSkinConfig, actionMode: ActionMode, actionElapsed: number) {
  const animation = config.model.animation;
  if (animation?.idle === false) return;
  const period = clamp(animation?.period ?? 3.2, 0.6, 12);
  const cycle = elapsed * Math.PI * 2 / period;
  const mode = actionMode;
  const bob = (animation?.bob ?? 0.014) * runtime.height * (mode === 2 ? 0.7 : 1);
  runtime.root.position.y += Math.sin(cycle) * bob;
  runtime.root.rotation.z = runtime.baseRotation.z + degrees((animation?.sway ?? 1.8) * Math.sin(cycle * (mode === 2 ? 0.35 : 0.5)));
  if (mode === 3) runtime.root.position.y += Math.max(0, Math.sin(actionElapsed * Math.PI * 2.2)) * runtime.height * 0.11;
  // Embedded clips own the normal idle pose. Procedural offsets are only an
  // additive interaction layer for a click gesture, or the fallback when a
  // skin has no clips at all.
  if (mode === 0 && runtime.actions.length) return;
  runtime.motionBones.forEach((motion, index) => {
    const phase = cycle * (index === 0 ? 1 : 0.72) + motion.phase;
    const wave = mode === 1 && motion.role === "rightArm" ? Math.sin(actionElapsed * 9) * degrees(28) : 0;
    const nod = mode === 2 && motion.role === "head" ? Math.sin(actionElapsed * 7) * degrees(18) : 0;
    const jump = mode === 3 && (motion.role === "leftArm" || motion.role === "rightArm")
      ? Math.sin(actionElapsed * 7) * degrees(16)
      : 0;
    const amplitude = motion.amplitude * (mode === 1 && motion.role === "rightArm" ? 2.5 : 1);
    const source = runtime.actions.length ? motion.node.quaternion.clone() : motion.base;
    const offset = new THREE.Quaternion().setFromEuler(new THREE.Euler(
      Math.sin(phase * 0.55) * amplitude * 0.35 + nod,
      Math.sin(phase * 0.7) * amplitude * 0.2,
      Math.sin(phase) * amplitude + wave + jump,
    ));
    motion.node.quaternion.copy(source).multiply(offset);
  });
}

function switchAnimation(runtime: ModelRuntime, nextIndex: number, loop: boolean) {
  if (!runtime.actions.length) return;
  const next = ((nextIndex % runtime.actions.length) + runtime.actions.length) % runtime.actions.length;
  if (next === runtime.activeAction && loop === runtime.activeLoop) return;
  const previous = runtime.actions[runtime.activeAction];
  previous?.fadeOut(0.18);
  runtime.actions[next].reset();
  runtime.actions[next].setLoop(loop ? THREE.LoopRepeat : THREE.LoopOnce, loop ? Infinity : 1);
  runtime.actions[next].clampWhenFinished = !loop;
  runtime.actions[next].fadeIn(0.18).play();
  runtime.activeAction = next;
  runtime.activeLoop = loop;
}

function findAction(actionNames: string[], pattern: RegExp): number | null {
  const index = actionNames.findIndex((name) => pattern.test(name));
  return index >= 0 ? index : null;
}

function actionForMode(runtime: ModelRuntime, mode: ActionMode): number {
  if (mode === 0) return runtime.actionMap.idle;
  if (mode === 1) return runtime.actionMap.wave ?? runtime.actionMap.idle;
  if (mode === 2) return runtime.actionMap.jump ?? runtime.actionMap.idle;
  return runtime.actionMap.walk ?? runtime.actionMap.idle;
}

function applyHeadTracking(runtime: ModelRuntime, gaze: Gaze, config: ModelPetSkinConfig) {
  const head = config.model.head;
  if (head?.enabled === false) return;
  const x = clamp((gaze?.x ?? 0) / 2.4, -1, 1);
  const y = clamp((gaze?.y ?? 0) / 2.4, -1, 1);
  const yaw = x * (head?.maxYaw ?? 24);
  const pitch = -y * (head?.maxPitch ?? 16);

  if (runtime.vrm?.lookAt) {
    // VRM's look-at API is intentionally angle based and drives eye bones or
    // expression-based eyes according to the model package.
    runtime.vrm.lookAt.yaw = yaw;
    runtime.vrm.lookAt.pitch = pitch;
    return;
  }

  const headNode = findBone(runtime.root, [head?.bone ?? "head"]);
  if (headNode) {
    headNode.rotation.y = degrees(yaw);
    headNode.rotation.x = degrees(pitch);
  }
}

function modelStyle(config: ModelPetSkinConfig, dock: ModelDock | null): ModelStyle {
  const pose = dock ? config.model.poses?.[dock] : undefined;
  return {
    "--pet-layout-size": `${config.layout.content}px`,
    "--pet-dock-right-shift": `${config.layout.dockShift.right}px`,
    "--pet-dock-left-shift": `${config.layout.dockShift.left}px`,
    "--pet-dock-top-shift": `${config.layout.dockShift.top}px`,
    "--pet-model-anchor-x": `${pose?.anchor?.[0] ?? 0}`,
    "--pet-model-anchor-y": `${pose?.anchor?.[1] ?? 0}`,
  };
}

/**
 * Lightweight runtime for an imported GLB/VRM skin. Generation is deliberately
 * outside the app; the installed app only carries the compact model and the
 * WebGL renderer.
 */
export function PetModel({ skin, config, dock, hovered, dragging, tokens, gaze, onAssetError }: PetModelProps) {
  const canvas = useRef<HTMLCanvasElement | null>(null);
  const modelElement = useRef<HTMLSpanElement | null>(null);
  const state = useRef<ModelState>({ dock, hovered, dragging, gaze });
  const actionMode = useRef<ActionMode>(0);
  const actionTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const interaction = useRef<ModelInteraction>({ pointerId: null, x: 0, y: 0, moved: false, yaw: 0, pitch: 0, hovering: false, hoverYaw: 0, hoverPitch: 0, actionMode: 0, actionStartedAt: 0 });
  const [selectedAction, setSelectedAction] = useState(0);
  const [ready, setReady] = useState(false);
  state.current = { dock, hovered, dragging, gaze };

  const onModelPointerDown = (event: React.PointerEvent<HTMLCanvasElement>) => {
    if (event.button !== 0) return;
    event.stopPropagation();
    updatePointerLook(interaction.current, event);
    interaction.current.pointerId = event.pointerId;
    interaction.current.x = event.clientX;
    interaction.current.y = event.clientY;
    interaction.current.moved = false;
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // A non-activating WebView can reject capture during a native window
      // transition. The hover path still rotates the model in that case.
    }
  };

  const onModelPointerMove = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const current = interaction.current;
    updatePointerLook(current, event);
    if (current.pointerId !== event.pointerId) return;
    event.stopPropagation();
    const dx = event.clientX - current.x;
    const dy = event.clientY - current.y;
    if (Math.abs(dx) > 1 || Math.abs(dy) > 1) current.moved = true;
    current.x = event.clientX;
    current.y = event.clientY;
    current.yaw = clamp(current.yaw + dx * 0.8, -180, 180);
    current.pitch = clamp(current.pitch - dy * 0.55, -38, 38);
  };

  const onModelPointerUp = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const current = interaction.current;
    if (current.pointerId !== event.pointerId) return;
    event.stopPropagation();
    if (!current.moved) {
      const next = ((actionMode.current + 1) % 4) as ActionMode;
      actionMode.current = next;
      current.actionMode = next;
      current.actionStartedAt = performance.now() / 1000;
      setSelectedAction(next);
      if (actionTimer.current !== null) clearTimeout(actionTimer.current);
      if (next !== 0) {
        actionTimer.current = setTimeout(() => {
          if (actionMode.current !== next) return;
          actionMode.current = 0;
          interaction.current.actionMode = 0;
          interaction.current.actionStartedAt = performance.now() / 1000;
          setSelectedAction(0);
        }, actionDurations[next]);
      }
    }
    current.pointerId = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
  };

  const onModelPointerLeave = () => {
    if (interaction.current.pointerId === null) interaction.current.hovering = false;
  };

  const onModelContextMenu = (event: React.MouseEvent<HTMLCanvasElement>) => event.preventDefault();

  useEffect(() => {
    const element = canvas.current;
    if (!element) return;
    let cancelled = false;
    let frame = 0;
    let resizeObserver: ResizeObserver | null = null;
    let runtime: ModelRuntime | null = null;
    let root: THREE.Object3D | null = null;
    let renderer: THREE.WebGLRenderer | null = null;

    setReady(false);
    actionMode.current = 0;
    setSelectedAction(0);
    interaction.current = { pointerId: null, x: 0, y: 0, moved: false, yaw: 0, pitch: 0, hovering: false, hoverYaw: 0, hoverPitch: 0, actionMode: 0, actionStartedAt: 0 };

    try {
      renderer = new THREE.WebGLRenderer({
        canvas: element,
        alpha: true,
        antialias: true,
        powerPreference: "low-power",
      });
      renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
      renderer.setClearColor(0x000000, 0);
      renderer.outputColorSpace = THREE.SRGBColorSpace;
      renderer.toneMapping = THREE.ACESFilmicToneMapping;
      renderer.toneMappingExposure = 1.12;

      const scene = new THREE.Scene();
      scene.add(new THREE.HemisphereLight(0xfff7ef, 0x6e7f9a, 2.2));
      const key = new THREE.DirectionalLight(0xffffff, 2.5);
      key.position.set(2, 3, 4);
      scene.add(key);
      const fill = new THREE.DirectionalLight(0x9edbff, 1.1);
      fill.position.set(-3, 1, 2);
      scene.add(fill);

      const camera = new THREE.PerspectiveCamera(24, 1, 0.01, 100);
      const target = new THREE.Vector3();
      const loader = new GLTFLoader();
      loader.register((parser) => new VRMLoaderPlugin(parser));

      const resize = () => {
        if (!renderer || !element.clientWidth || !element.clientHeight) return;
        renderer.setSize(element.clientWidth, element.clientHeight, false);
        camera.aspect = element.clientWidth / element.clientHeight;
        camera.updateProjectionMatrix();
      };
      resizeObserver = typeof ResizeObserver !== "undefined" ? new ResizeObserver(resize) : null;
      resizeObserver?.observe(element);
      resize();

      void loader.loadAsync(config.model.url).then((gltf) => {
        if (cancelled || !renderer) {
          disposeModel(gltf.scene);
          return;
        }
        const vrm = gltf.userData.vrm as VRM | undefined;
        root = vrm?.scene ?? gltf.scene;
        const fitted = normalizeModel(root, config);
        scene.add(root);
        const clipName = config.model.animation?.clip?.trim().toLowerCase();
        const mixer = gltf.animations.length ? new THREE.AnimationMixer(root) : undefined;
        const actions = mixer ? gltf.animations.map((clip) => mixer.clipAction(clip)) : [];
        const actionNames = gltf.animations.map((clip) => clip.name);
        const configuredIdle = clipName ? actionNames.findIndex((name) => name.toLowerCase() === clipName) : -1;
        const idleAction = actions.length
          ? (configuredIdle >= 0 ? configuredIdle : findAction(actionNames, /idle|breath|survey|stand|loop/i) ?? 0)
          : -1;
        runtime = {
          root,
          vrm,
          mixer,
          actions,
          actionNames,
          actionMap: {
            idle: idleAction,
            wave: findAction(actionNames, /wave|punch|dance|working|attack|gesture/i),
            jump: findAction(actionNames, /jump|sit(?:down)?|standup|hop/i),
            walk: findAction(actionNames, /run|walk/i),
          },
          activeAction: -1,
          activeLoop: false,
          motionBones: createMotionBones(root, vrm),
          height: fitted.height,
          width: fitted.width,
          depth: fitted.depth,
          baseRotation: fitted.rotation,
          basePosition: fitted.position,
        };
        if (actions.length) switchAnimation(runtime, idleAction, true);
        setReady(true);

        const clock = new THREE.Clock();
        let nextBlink = 4.5;
        let blinkAt = -1;
        const render = () => {
          if (cancelled || !renderer || !runtime) return;
          const delta = Math.min(clock.getDelta(), 0.05);
          const current = state.current;
          const pose = current.dock ? config.model.poses?.[current.dock] : undefined;
          const cameraConfig = config.model.camera;
          const fov = cameraConfig?.fov ?? 24;
          const aspect = Math.max(element.clientWidth / Math.max(element.clientHeight, 1), 0.25);
          const horizontalFov = 2 * Math.atan(Math.tan(degrees(fov) / 2) * aspect);
          const targetY = cameraConfig?.targetY ?? runtime.height * 0.5;
          const verticalHalf = Math.max(targetY, runtime.height - targetY);
          const horizontalHalf = Math.max(runtime.width, runtime.depth) * 0.5;
          const fitDistance = Math.max(
            verticalHalf / Math.tan(degrees(fov) / 2),
            horizontalHalf / Math.tan(horizontalFov / 2),
          ) * 1.34;
          const distance = pose?.cameraDistance ?? cameraConfig?.distance ?? fitDistance;
          const yaw = pose?.cameraYaw ?? 0;
          const pitch = pose?.cameraPitch ?? 0;
          target.set(0, targetY, 0);
          camera.fov = fov;
          camera.position.set(
            Math.sin(degrees(yaw)) * distance,
            targetY + Math.sin(degrees(pitch)) * distance,
            Math.cos(degrees(yaw)) * distance,
          );
          camera.lookAt(target);
          camera.updateProjectionMatrix();

          const cursorYaw = current.gaze ? clamp(current.gaze.x * 14, -34, 34) : 0;
          const cursorPitch = current.gaze ? clamp(-current.gaze.y * 9, -22, 22) : 0;
          const pointerYaw = interaction.current.pointerId === null
            ? (interaction.current.hovering ? interaction.current.hoverYaw : cursorYaw)
            : 0;
          const pointerPitch = interaction.current.pointerId === null
            ? (interaction.current.hovering ? interaction.current.hoverPitch : cursorPitch)
            : 0;
          runtime.root.rotation.copy(runtime.baseRotation);
          runtime.root.rotation.y += degrees((pose?.rotationY ?? 0) + interaction.current.yaw + pointerYaw);
          runtime.root.rotation.x += degrees(interaction.current.pitch + pointerPitch);
          runtime.root.position.copy(runtime.basePosition);
          runtime.root.position.x += (pose?.anchor?.[0] ?? 0) * runtime.height;
          runtime.root.position.y += (pose?.anchor?.[1] ?? 0) * runtime.height;
          const mode = actionMode.current;
          const requestedAction = actionForMode(runtime, mode);
          const embeddedGesture = mode > 0 && requestedAction !== runtime.actionMap.idle;
          const loop = mode === 0 || mode === 3 || !embeddedGesture;
          const actionElapsed = Math.max(0, performance.now() / 1000 - interaction.current.actionStartedAt);
          switchAnimation(runtime, requestedAction, loop);
          runtime.mixer?.update(delta);
          applyProceduralMotion(runtime, clock.elapsedTime, config, mode, actionElapsed);
          applyHeadTracking(runtime, current.gaze, config);
          modelElement.current?.setAttribute("data-animation", runtime.actionNames[runtime.activeAction] ?? "procedural");
          modelElement.current?.setAttribute("data-action-state", mode === 0 ? "idle" : mode === 1 ? "wave" : mode === 2 ? "jump" : "walk");
          if (runtime.vrm) {
            const expressions = runtime.vrm.expressionManager;
            if (expressions && config.model.animation?.blink !== false) {
              const now = clock.elapsedTime;
              if (blinkAt < 0 && now >= nextBlink) blinkAt = now;
              let blink = 0;
              if (blinkAt >= 0) {
                const progress = (now - blinkAt) / 0.18;
                if (progress >= 1) {
                  blinkAt = -1;
                  nextBlink = now + 4.5 + Math.random() * 3.5;
                } else blink = progress < .5 ? progress * 2 : (1 - progress) * 2;
              }
              if (expressions.getExpression("blink")) expressions.setValue("blink", blink);
              const happy = expressions.getExpression("happy") ?? expressions.getExpression("joy");
              if (happy) expressions.setValue(happy.expressionName, current.hovered && !current.dragging ? 1 : 0);
            }
            runtime.vrm.update(delta);
          }
          renderer.render(scene, camera);
          frame = requestAnimationFrame(render);
        };
        render();
      }).catch(() => {
        if (!cancelled) onAssetError();
      });
    } catch {
      onAssetError();
    }

    return () => {
      cancelled = true;
      if (actionTimer.current !== null) clearTimeout(actionTimer.current);
      if (frame) cancelAnimationFrame(frame);
      resizeObserver?.disconnect();
      if (runtime?.root) disposeModel(runtime.root);
      else if (root) disposeModel(root);
      renderer?.dispose();
      renderer?.forceContextLoss();
    };
  }, [config, onAssetError]);

  const pose = dock ?? "none";
  return (
    <span ref={modelElement} className="pet-skin pet-model" data-skin={skin} data-dock={pose}
      data-motion={dragging ? "drag" : hovered ? "happy" : "idle"}
      data-action={selectedAction}
      data-ready={ready} style={modelStyle(config, dock)} aria-hidden="true">
      <span className="pet-pose">
        <canvas ref={canvas} className="pet-model-canvas" aria-hidden="true"
          title="拖动旋转模型，点击切换动作"
          onPointerDown={onModelPointerDown}
          onPointerMove={onModelPointerMove}
          onPointerUp={onModelPointerUp}
          onPointerCancel={onModelPointerUp}
          onPointerEnter={onModelPointerMove}
          onPointerLeave={onModelPointerLeave}
          onContextMenu={onModelContextMenu} />
      </span>
      <span className="pet-model-hint">{t("bubble.pet_model_hint")}</span>
      <span className="pet-token-badge">
        <strong>{ballTokens(tokens)}</strong>
        <small>{t("bubble.today")} tokens</small>
      </span>
    </span>
  );
}
