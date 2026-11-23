// Signs posts the way a browser running src/player.js does, for the tests in src/forms.rs.
// Reads a JSON list of requests on stdin and prints, for each, the fields the form would
// post, or {error} with what the page said instead.
//
// A request is {kind, fields: [[name, value], ...], pass, at, bound} for a form, `bound`
// being the key the site says the callsign plays with; or {kind, raw, call, pass} to
// sign the payload `raw` as it is, the way a client other than the page could.
import fs from "node:fs";
import vm from "node:vm";

const src = fs.readFileSync(process.argv[2], "utf8");
const requests = JSON.parse(fs.readFileSync(0, "utf8"));

function page(globals) {
  const ctx = vm.createContext({ TextEncoder, setTimeout, addEventListener() {}, ...globals });
  vm.runInContext(src, ctx);
  return ctx;
}

function post(r) {
  return new Promise((resolve) => {
    const els = r.fields.map(([name, value]) => ({ name, value }));
    for (const e of els) els[e.name] = e;
    const note = { textContent: "" };
    // the page re-enables the button only when it stops short of posting
    const button = {
      set disabled(v) {
        if (!v) resolve({ error: note.textContent });
      },
    };
    const parts = {
      ".signing": note,
      "button[type=submit]": button,
      "[data-pass]": { value: r.pass },
      "[data-at]": { value: r.at ?? "" },
      "[data-from]": { value: "" },
    };
    let onSubmit;
    const form = {
      elements: els,
      dataset: { sign: r.kind },
      addEventListener(type, fn) {
        if (type === "submit") onSubmit = fn;
      },
      querySelector: (sel) => parts[sel] ?? null,
      submit() {
        resolve(Object.fromEntries(els.map((e) => [e.name, e.value])));
      },
    };
    const fetch = async () => ({ ok: true, text: async () => r.bound ?? "" });
    page({ document: { querySelectorAll: () => [form] }, fetch });
    onSubmit({ target: form, preventDefault() {} });
  });
}

function raw(r) {
  const { rwg } = page({});
  const key = rwg.keyFrom(rwg.seedFrom(r.call, r.pass));
  const p = { ...r.raw, pk: rwg.hex(key.pub) };
  p.psig = rwg.hex(rwg.sign(key, rwg.utf8(rwg.signedText(r.kind, p))));
  return p;
}

const out = [];
for (const r of requests) out.push(r.raw ? raw(r) : await post(r));
process.stdout.write(JSON.stringify(out));
