#!/usr/bin/env node
// Muestra el DOM YA RENDERIZADO de una página, manejando el Chrome que corre al
// lado por CDP.
//
// Existe por un límite concreto: la ronda de autoría de recetas le pide a
// Claude que mire un portal de facturación y escriba los locators. Los portales
// son aplicaciones de una sola página —arman el formulario con JavaScript— y la
// herramienta que Claude trae de fábrica (WebFetch) sólo baja el HTML inicial,
// donde el formulario todavía no existe. Medido contra un portal real: "queda
// en estado Loading...".
//
// Sin dependencias a propósito: Node 22 ya trae `fetch` y `WebSocket`, y una
// imagen de producción con menos paquetes es una imagen con menos parches
// pendientes.
//
//   pw-dom <url> [--esperar ms] [--texto] [--campos]
//
//   --esperar  cuánto darle al JS después del load (por omisión 2500 ms)
//   --texto    el texto visible en vez del HTML
//   --campos   sólo los campos de formulario, que es lo que la autoría busca

const args = process.argv.slice(2);
const url = args.find((a) => !a.startsWith("--"));
if (!url) {
  console.error("uso: pw-dom <url> [--esperar ms] [--texto] [--campos]");
  process.exit(2);
}
const flag = (n, def) => {
  const i = args.indexOf(`--${n}`);
  return i === -1 ? def : args[i + 1];
};
const espera = Number(flag("esperar", 2500));
const quiereTexto = args.includes("--texto");
const quiereCampos = args.includes("--campos");
const CDP = process.env.PW_CDP || "http://127.0.0.1:9222";

const fatal = (m) => {
  console.error(`pw-dom: ${m}`);
  process.exit(1);
};

// Una pestaña nueva por corrida, y se cierra al final: dos rondas en paralelo
// no se pisan, y no se van acumulando pestañas en un Chrome que vive días.
let objetivo;
try {
  const r = await fetch(`${CDP}/json/new?url=about:blank`, { method: "PUT" });
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  objetivo = await r.json();
} catch (e) {
  fatal(`no pude abrir una pestaña en ${CDP}: ${e.message}`);
}

const ws = new WebSocket(objetivo.webSocketDebuggerUrl);
let n = 0;
const pendientes = new Map();
const esperados = new Map();

const llamar = (method, params = {}) =>
  new Promise((res, rej) => {
    const id = ++n;
    pendientes.set(id, { res, rej });
    ws.send(JSON.stringify({ id, method, params }));
  });

const evento = (nombre, ms) =>
  new Promise((res) => {
    const t = setTimeout(() => res(false), ms);
    esperados.set(nombre, () => {
      clearTimeout(t);
      res(true);
    });
  });

ws.addEventListener("message", (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pendientes.has(msg.id)) {
    const { res, rej } = pendientes.get(msg.id);
    pendientes.delete(msg.id);
    msg.error ? rej(new Error(msg.error.message)) : res(msg.result);
  } else if (msg.method && esperados.has(msg.method)) {
    esperados.get(msg.method)();
    esperados.delete(msg.method);
  }
});

const cerrar = async () => {
  try {
    ws.close();
  } catch {}
  try {
    await fetch(`${CDP}/json/close/${objetivo.id}`);
  } catch {}
};

try {
  await new Promise((res, rej) => {
    ws.addEventListener("open", res, { once: true });
    ws.addEventListener("error", () => rej(new Error("no se pudo abrir el websocket")), {
      once: true,
    });
  });

  await llamar("Page.enable");
  const cargo = evento("Page.loadEventFired", 30000);
  await llamar("Page.navigate", { url });
  const hubo = await cargo;
  if (!hubo) console.error("pw-dom: aviso — la página no terminó de cargar en 30 s, sigo igual");

  // El load dispara antes de que el JS arme la interfaz: sin esta espera se ve
  // el mismo "Loading..." que ve WebFetch, que es todo el punto de este script.
  await new Promise((r) => setTimeout(r, espera));

  const expr = quiereCampos
    ? `JSON.stringify([...document.querySelectorAll('input,select,textarea,button')].map(e=>({
         etiqueta: e.tagName.toLowerCase(), tipo: e.type||null, nombre: e.name||null,
         id: e.id||null, placeholder: e.placeholder||null,
         texto: (e.innerText||e.value||'').trim().slice(0,60) || null,
         visible: !!(e.offsetWidth||e.offsetHeight)
       })), null, 1)`
    : quiereTexto
      ? "document.body.innerText"
      : "document.documentElement.outerHTML";

  const { result } = await llamar("Runtime.evaluate", {
    expression: expr,
    returnByValue: true,
    awaitPromise: true,
  });
  const salida = String(result.value ?? "");
  console.log(salida.length > 200000 ? `${salida.slice(0, 200000)}\n… (recortado)` : salida);
} catch (e) {
  await cerrar();
  fatal(e.message);
}
await cerrar();
