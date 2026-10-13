// The page's start-up: fonts, the keyboard-aware layout, the
// service worker, the WebGPU check, then the app (cce_vault_web.js).
import init, { start } from './cce_vault_web.js';

const note = (text) => {
  const el = document.getElementById('note');
  el.textContent = text;
  el.style.display = 'grid';
};

// The fonts the app draws with (all SIL OFL; fallback is the first face
// in this order that has the glyph): the generic serif,
// sans-serif and monospace families, named in this order below.
const FONTS = [
  'fonts/NotoSans-Regular.ttf', 'fonts/NotoSans-Bold.ttf',
  'fonts/NotoSans-Italic.ttf', 'fonts/NotoSans-BoldItalic.ttf',
  'fonts/NotoSerif-Regular.ttf', 'fonts/NotoSerif-Bold.ttf',
  'fonts/JetBrainsMono-Regular.ttf', 'fonts/JetBrainsMono-Bold.ttf',
  // Last: the fallback for symbols the text faces lack (📅).
  'fonts/fa-regular-400.ttf',
];
const FAMILIES = 'Noto Serif,Noto Sans,JetBrains Mono';

// An on-screen keyboard covers the page rather than resizing it (iOS):
// follow the visual viewport, so the canvas ends where the keyboard
// begins and the editor's caret stays in view.
const vv = window.visualViewport;
if (vv) {
  const fit = () => {
    const kb = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
    document.documentElement.style.setProperty('--kb', kb + 'px');
    if (window.scrollY) window.scrollTo(0, 0);
  };
  vv.addEventListener('resize', fit);
  vv.addEventListener('scroll', fit);
}

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('sw.js').catch((e) => console.warn('service worker:', e));
}

const adapter = navigator.gpu && await navigator.gpu.requestAdapter().catch(() => null);
if (!adapter) {
  note('This app draws with WebGPU, which this browser does not offer. On an iPhone it needs iOS 26 or later (Safari 26).');
} else {
  try {
    await init();
    const fonts = await Promise.all(FONTS.map(async (f) => new Uint8Array(await (await fetch(f)).arrayBuffer())));
    await start(document.getElementById('app'), fonts, FAMILIES);
  } catch (e) {
    console.error(e);
    note('The app failed to start: ' + e);
  }
}
