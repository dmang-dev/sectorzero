// Translations. The English text is the key: `t('Inject files')`, with `{name}` placeholders
// filled from an object. A catalog (locales/xx.js) sets
// `window.SZ_LOCALES['xx'] = { 'English text': 'translation', … }`; anything missing stays
// English. Static text in index.html is translated in place by `translatePage()`, except inside
// elements marked data-no-i18n; an element marked data-i18n-html is translated as one HTML
// string (for text with markup inside). `node app/i18n-check.mjs` checks the catalogs.
'use strict';

window.SZ_LOCALES = window.SZ_LOCALES || {};

// [code, name in that language]. English first, then by name.
const LANGUAGES = [
  ['en', 'English'],
  ['cs', 'Čeština'],
  ['de', 'Deutsch'],
  ['es', 'Español'],
  ['fr', 'Français'],
  ['it', 'Italiano'],
  ['hu', 'Magyar'],
  ['nl', 'Nederlands'],
  ['pl', 'Polski'],
  ['pt-BR', 'Português (Brasil)'],
  ['tr', 'Türkçe'],
  ['ru', 'Русский'],
  ['uk', 'Українська'],
  ['ja', '日本語'],
  ['ko', '한국어'],
  ['zh-CN', '简体中文'],
  ['zh-TW', '繁體中文'],
];

const I18N = {
  lang: 'en',
  dict: {},

  // Best supported language for a BCP 47 tag ('de-AT' → 'de', 'zh-HK' → 'zh-TW').
  match(tag) {
    if (!tag) return null;
    const lower = tag.toLowerCase();
    const codes = LANGUAGES.map(([c]) => c);
    const exact = codes.find((c) => c.toLowerCase() === lower);
    if (exact) return exact;
    const base = lower.split('-')[0];
    if (base === 'zh') return /hant|tw|hk|mo/.test(lower) ? 'zh-TW' : 'zh-CN';
    if (base === 'pt') return 'pt-BR';
    return codes.find((c) => c.split('-')[0] === base) ?? null;
  },

  // The saved choice, else the system's preferred languages, else English.
  preferred() {
    let saved = null;
    try { saved = localStorage.getItem('sz-lang'); } catch { /* storage unavailable */ }
    if (saved && this.match(saved)) return this.match(saved);
    for (const tag of navigator.languages ?? [navigator.language]) {
      const m = this.match(tag);
      if (m) return m;
    }
    return 'en';
  },

  remember(lang) {
    try { localStorage.setItem('sz-lang', lang); } catch { /* storage unavailable */ }
  },

  async load(lang) {
    if (lang !== 'en' && !window.SZ_LOCALES[lang]) {
      await new Promise((resolve) => {
        const s = document.createElement('script');
        s.src = `locales/${lang}.js`;
        s.onload = resolve;
        s.onerror = resolve; // a missing catalog falls back to English
        document.head.append(s);
      });
    }
    this.lang = window.SZ_LOCALES[lang] || lang === 'en' ? lang : 'en';
    this.dict = this.lang === 'en' ? {} : window.SZ_LOCALES[this.lang];
    document.documentElement.lang = this.lang;
  },

  t(text, vars) {
    const s = Object.hasOwn(this.dict, text) ? this.dict[text] : text;
    return vars ? s.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? String(vars[k]) : m)) : s;
  },
};

const t = (text, vars) => I18N.t(text, vars);

// ---------- static page text ----------

const I18N_ATTRS = ['title', 'placeholder', 'aria-label'];
const i18nNorm = (s) => s.replace(/\s+/g, ' ').trim();
// Recorded on the first pass so a later language change starts again from the English.
let i18nTexts = null;

function i18nCollect(root, texts) {
  for (const el of root.querySelectorAll('[data-i18n-html]')) {
    texts.push({ html: el, src: i18nNorm(el.innerHTML) });
  }
  for (const el of root.querySelectorAll('*')) {
    if (el.closest('[data-no-i18n]')) continue;
    for (const a of I18N_ATTRS) {
      const v = el.getAttribute(a);
      if (v && i18nNorm(v)) texts.push({ el, attr: a, src: i18nNorm(v) });
    }
  }
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const p = n.parentElement;
    if (p && (p.closest('script, style, title, [data-no-i18n], [data-i18n-html]'))) continue;
    const src = i18nNorm(n.nodeValue);
    if (!src) continue;
    const [, lead, trail] = n.nodeValue.match(/^(\s*)[\s\S]*?(\s*)$/);
    texts.push({ node: n, src, lead, trail });
  }
  for (const tpl of root.querySelectorAll('template')) i18nCollect(tpl.content, texts);
}

function translatePage() {
  if (!i18nTexts) {
    i18nTexts = [];
    i18nCollect(document, i18nTexts);
  }
  for (const x of i18nTexts) {
    if (x.html) x.html.innerHTML = t(x.src);
    else if (x.attr) x.el.setAttribute(x.attr, t(x.src));
    else x.node.nodeValue = x.lead + t(x.src) + x.trail;
  }
}
