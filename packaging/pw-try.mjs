#!/usr/bin/env node
// Prueba una receta KDL contra el portal real, EN SECO: corre todos los pasos hasta el que lleva
// `commit=#true` (el botón final que emite la factura) y se detiene ANTES de él. El daemon se niega
// a correr una receta sin esa marca, así que esta herramienta no puede emitir nada.
//
// Existe para la ronda de autoría: `pw-dom` sólo muestra una página; un portal de facturación es un
// flujo de varias pantallas (folio → buscar → datos fiscales → emitir). Con `pw-try` el modelo
// escribe la receta, la corre, lee dónde falló y la corrige, en la misma ronda.
//
//   pw-try <receta.kdl> [--vars '{"folio":"123"}' | --vars-file vars.json] [--todo]
//
//   --todo   imprime el árbol de accesibilidad completo (por omisión se recorta)
//
// Habla con el daemon por su API HTTP (`PW_API`, o `PACEWRIGHT_WEB_ADDR`, por omisión
// 127.0.0.1:7878) con el método `try_src`, que corre sincrónico y fuera de la cola.

import { readFileSync } from "node:fs";

const args = process.argv.slice(2);
const file = args.find((a) => !a.startsWith("--") && !a.startsWith("{"));
const flag = (n) => {
  const i = args.indexOf(`--${n}`);
  return i === -1 ? undefined : args[i + 1];
};
if (!file) {
  console.error("uso: pw-try <receta.kdl> [--vars JSON | --vars-file archivo.json] [--todo]");
  process.exit(2);
}
const fatal = (m) => {
  console.error(`pw-try: ${m}`);
  process.exit(1);
};

const base = (() => {
  const raw = (process.env.PW_API || process.env.PACEWRIGHT_WEB_ADDR || "127.0.0.1:7878").trim();
  return (/^[a-z][a-z0-9+.-]*:\/\//i.test(raw) ? raw : `http://${raw}`).replace(/\/+$/, "");
})();

let src;
try {
  src = readFileSync(file, "utf8");
} catch (e) {
  fatal(`no pude leer ${file}: ${e.message}`);
}
let vars = {};
try {
  const raw = flag("vars-file") ? readFileSync(flag("vars-file"), "utf8") : flag("vars");
  if (raw) vars = JSON.parse(raw);
} catch (e) {
  fatal(`las vars no son un objeto JSON: ${e.message}`);
}

const todo = args.includes("--todo");
const cut = (s, n) => (typeof s === "string" && !todo && s.length > n ? `${s.slice(0, n)}\n… (recortado)` : s);

let res;
try {
  const r = await fetch(`${base}/api`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ method: "try_src", params: { recipe_src: src, params: vars } }),
  });
  res = await r.json();
} catch (e) {
  fatal(`el daemon no contestó en ${base}: ${e.message}. Es una falla de la herramienta, repórtala tal cual.`);
}
if (res.type === "error") fatal(res.message);

const page = (p) =>
  p
    ? [
        `url: ${p.url ?? "?"}`,
        `título: ${p.title ?? "?"}`,
        "--- texto de la página ---",
        cut(p.page_text ?? "", 6000),
        "--- árbol de accesibilidad ---",
        cut(p.ax_tree ?? "", 8000),
      ].join("\n")
    : "(sin captura de la página)";

if (res.ok) {
  const d = res.dry_run || {};
  console.log(`OK: la receta llegó al paso ${Number(d.stopped_before) + 1} (commit) y se detuvo ANTES de él.`);
  console.log(`paso no ejecutado: ${d.step}`);
  console.log(`capturas: ${JSON.stringify(res.result)}`);
  if (res.unexpected?.length) console.log(`avisos: ${res.unexpected.join("; ")}`);
  console.log(page(d.page));
  process.exit(0);
}
const f = res.failure || {};
console.log(`FALLÓ: ${res.error}`);
if (f.step_index != null) console.log(`en el paso ${f.step_index + 1}: ${f.step}`);
console.log(page(f));
process.exit(3);
