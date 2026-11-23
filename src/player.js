// Signs each game post with the player's own Ed25519 key (RFC 8032). The key is made on
// this device from the callsign and a passphrase, which never leave the page. Pages on
// the mesh are plain HTTP, where browsers offer no WebCrypto, so the math is done here.
(function (root) {
  "use strict";

  var K = new Int32Array([
    0x428a2f98, 0xd728ae22, 0x71374491, 0x23ef65cd, 0xb5c0fbcf, 0xec4d3b2f, 0xe9b5dba5, 0x8189dbbc,
    0x3956c25b, 0xf348b538, 0x59f111f1, 0xb605d019, 0x923f82a4, 0xaf194f9b, 0xab1c5ed5, 0xda6d8118,
    0xd807aa98, 0xa3030242, 0x12835b01, 0x45706fbe, 0x243185be, 0x4ee4b28c, 0x550c7dc3, 0xd5ffb4e2,
    0x72be5d74, 0xf27b896f, 0x80deb1fe, 0x3b1696b1, 0x9bdc06a7, 0x25c71235, 0xc19bf174, 0xcf692694,
    0xe49b69c1, 0x9ef14ad2, 0xefbe4786, 0x384f25e3, 0x0fc19dc6, 0x8b8cd5b5, 0x240ca1cc, 0x77ac9c65,
    0x2de92c6f, 0x592b0275, 0x4a7484aa, 0x6ea6e483, 0x5cb0a9dc, 0xbd41fbd4, 0x76f988da, 0x831153b5,
    0x983e5152, 0xee66dfab, 0xa831c66d, 0x2db43210, 0xb00327c8, 0x98fb213f, 0xbf597fc7, 0xbeef0ee4,
    0xc6e00bf3, 0x3da88fc2, 0xd5a79147, 0x930aa725, 0x06ca6351, 0xe003826f, 0x14292967, 0x0a0e6e70,
    0x27b70a85, 0x46d22ffc, 0x2e1b2138, 0x5c26c926, 0x4d2c6dfc, 0x5ac42aed, 0x53380d13, 0x9d95b3df,
    0x650a7354, 0x8baf63de, 0x766a0abb, 0x3c77b2a8, 0x81c2c92e, 0x47edaee6, 0x92722c85, 0x1482353b,
    0xa2bfe8a1, 0x4cf10364, 0xa81a664b, 0xbc423001, 0xc24b8b70, 0xd0f89791, 0xc76c51a3, 0x0654be30,
    0xd192e819, 0xd6ef5218, 0xd6990624, 0x5565a910, 0xf40e3585, 0x5771202a, 0x106aa070, 0x32bbd1b8,
    0x19a4c116, 0xb8d2d0c8, 0x1e376c08, 0x5141ab53, 0x2748774c, 0xdf8eeb99, 0x34b0bcb5, 0xe19b48a8,
    0x391c0cb3, 0xc5c95a63, 0x4ed8aa4a, 0xe3418acb, 0x5b9cca4f, 0x7763e373, 0x682e6ff3, 0xd6b2b8a3,
    0x748f82ee, 0x5defb2fc, 0x78a5636f, 0x43172f60, 0x84c87814, 0xa1f0ab72, 0x8cc70208, 0x1a6439ec,
    0x90befffa, 0x23631e28, 0xa4506ceb, 0xde82bde9, 0xbef9a3f7, 0xb2c67915, 0xc67178f2, 0xe372532b,
    0xca273ece, 0xea26619c, 0xd186b8c7, 0x21c0c207, 0xeada7dd6, 0xcde0eb1e, 0xf57d4f7f, 0xee6ed178,
    0x06f067aa, 0x72176fba, 0x0a637dc5, 0xa2c898a6, 0x113f9804, 0xbef90dae, 0x1b710b35, 0x131c471b,
    0x28db77f5, 0x23047d84, 0x32caab7b, 0x40c72493, 0x3c9ebe0a, 0x15c9bebc, 0x431d67c4, 0x9c100d4c,
    0x4cc5d4be, 0xcb3e42b6, 0x597f299c, 0xfc657e2a, 0x5fcb6fab, 0x3ad6faec, 0x6c44198c, 0x4a475817
  ]);
  var H0 = new Int32Array([
    0x6a09e667, 0xf3bcc908, 0xbb67ae85, 0x84caa73b, 0x3c6ef372, 0xfe94f82b, 0xa54ff53a, 0x5f1d36f1,
    0x510e527f, 0xade682d1, 0x9b05688c, 0x2b3e6c1f, 0x1f83d9ab, 0xfb41bd6b, 0x5be0cd19, 0x137e2179
  ]);
  var W = new Int32Array(160);

  // 64-bit words are kept as (high, low) pairs of 32-bit ints; sums carry through the low half
  function sha512(msg) {
    var n = msg.length, blocks = (n + 17 + 127) >> 7;
    var buf = new Uint8Array(blocks * 128);
    buf.set(msg);
    buf[n] = 0x80;
    var bits = n * 8, end = blocks * 128;
    put32(buf, end - 8, Math.floor(bits / 4294967296));
    put32(buf, end - 4, bits >>> 0);
    var h = new Int32Array(H0);
    for (var off = 0; off < end; off += 128) {
      var i;
      for (i = 0; i < 32; i++) {
        W[i] = (buf[off + 4 * i] << 24) | (buf[off + 4 * i + 1] << 16) | (buf[off + 4 * i + 2] << 8) | buf[off + 4 * i + 3];
      }
      for (i = 32; i < 160; i += 2) {
        var xh = W[i - 30], xl = W[i - 29], yh = W[i - 4], yl = W[i - 3];
        var s0h = ((xh >>> 1) | (xl << 31)) ^ ((xh >>> 8) | (xl << 24)) ^ (xh >>> 7);
        var s0l = ((xl >>> 1) | (xh << 31)) ^ ((xl >>> 8) | (xh << 24)) ^ ((xl >>> 7) | (xh << 25));
        var s1h = ((yh >>> 19) | (yl << 13)) ^ ((yl >>> 29) | (yh << 3)) ^ (yh >>> 6);
        var s1l = ((yl >>> 19) | (yh << 13)) ^ ((yh >>> 29) | (yl << 3)) ^ ((yl >>> 6) | (yh << 26));
        var lo = (W[i - 31] >>> 0) + (s0l >>> 0) + (W[i - 13] >>> 0) + (s1l >>> 0);
        W[i] = W[i - 32] + s0h + W[i - 14] + s1h + ((lo / 4294967296) | 0);
        W[i + 1] = lo;
      }
      var ah = h[0], al = h[1], bh = h[2], bl = h[3], ch = h[4], cl = h[5], dh = h[6], dl = h[7];
      var eh = h[8], el = h[9], fh = h[10], fl = h[11], gh = h[12], gl = h[13], hh = h[14], hl = h[15];
      for (i = 0; i < 160; i += 2) {
        var S1h = ((eh >>> 14) | (el << 18)) ^ ((eh >>> 18) | (el << 14)) ^ ((el >>> 9) | (eh << 23));
        var S1l = ((el >>> 14) | (eh << 18)) ^ ((el >>> 18) | (eh << 14)) ^ ((eh >>> 9) | (el << 23));
        var chh = (eh & fh) ^ (~eh & gh), chl = (el & fl) ^ (~el & gl);
        var t1l = (hl >>> 0) + (S1l >>> 0) + (chl >>> 0) + (K[i + 1] >>> 0) + (W[i + 1] >>> 0);
        var t1h = hh + S1h + chh + K[i] + W[i] + ((t1l / 4294967296) | 0);
        t1l = t1l >>> 0;
        var S0h = ((ah >>> 28) | (al << 4)) ^ ((al >>> 2) | (ah << 30)) ^ ((al >>> 7) | (ah << 25));
        var S0l = ((al >>> 28) | (ah << 4)) ^ ((ah >>> 2) | (al << 30)) ^ ((ah >>> 7) | (al << 25));
        var mjh = (ah & bh) ^ (ah & ch) ^ (bh & ch), mjl = (al & bl) ^ (al & cl) ^ (bl & cl);
        var t2l = (S0l >>> 0) + (mjl >>> 0);
        var t2h = S0h + mjh + ((t2l / 4294967296) | 0);
        hh = gh; hl = gl; gh = fh; gl = fl; fh = eh; fl = el;
        var e = (dl >>> 0) + t1l;
        eh = (dh + t1h + ((e / 4294967296) | 0)) | 0;
        el = e | 0;
        dh = ch; dl = cl; ch = bh; cl = bl; bh = ah; bl = al;
        var a = t1l + (t2l >>> 0);
        ah = (t1h + t2h + ((a / 4294967296) | 0)) | 0;
        al = a | 0;
      }
      fold(h, 0, ah, al);
      fold(h, 2, bh, bl);
      fold(h, 4, ch, cl);
      fold(h, 6, dh, dl);
      fold(h, 8, eh, el);
      fold(h, 10, fh, fl);
      fold(h, 12, gh, gl);
      fold(h, 14, hh, hl);
    }
    var out = new Uint8Array(64);
    for (var j = 0; j < 16; j++) put32(out, 4 * j, h[j]);
    return out;
  }

  function fold(h, i, xh, xl) {
    var s = (h[i + 1] >>> 0) + (xl >>> 0);
    h[i] = h[i] + xh + ((s / 4294967296) | 0);
    h[i + 1] = s;
  }

  function put32(b, at, x) {
    b[at] = x >>> 24;
    b[at + 1] = x >>> 16;
    b[at + 2] = x >>> 8;
    b[at + 3] = x;
  }

  // no BigInt literals, so the script still loads where BigInt is missing (iOS before 14)
  // and can say so; the curve's numbers are made on first use
  var big = root.BigInt, N0, N1, N2, N8, N255, P, L, D2, BASE;

  function curve() {
    if (P) return;
    N0 = big(0);
    N1 = big(1);
    N2 = big(2);
    N8 = big(8);
    N255 = big(255);
    P = (N1 << N255) - big(19);
    L = (N1 << big(252)) + big("27742317777372353535851937790883648493");
    D2 = mod(N2 * big(-121665) * inv(big(121666)));
    var x = big("15112221349535400772501151409588531511454012693041857206046113283949847762202");
    var y = big("46316835694926478169428394003475163141307993866256225615783033603165251855960");
    BASE = [x, y, N1, mod(x * y)];
  }

  function mod(a, m) {
    var r = a % (m || P);
    return r < N0 ? r + (m || P) : r;
  }

  function inv(a) {
    var r = N1, e = P - N2;
    a = mod(a);
    while (e > N0) {
      if (e & N1) r = mod(r * a);
      a = mod(a * a);
      e >>= N1;
    }
    return r;
  }

  // points in extended coordinates [X, Y, Z, T]; RFC 8032's addition, which also doubles
  function add(p, q) {
    var a = mod((p[1] - p[0]) * (q[1] - q[0])), b = mod((p[1] + p[0]) * (q[1] + q[0]));
    var c = mod(p[3] * D2 * q[3]), d = mod(N2 * p[2] * q[2]);
    var e = b - a, f = d - c, g = d + c, h = b + a;
    return [mod(e * f), mod(g * h), mod(f * g), mod(e * h)];
  }

  function times(n, p) {
    var r = [N0, N1, N1, N0];
    for (; n > N0; n >>= N1) {
      if (n & N1) r = add(r, p);
      p = add(p, p);
    }
    return r;
  }

  function encode(p) {
    var zi = inv(p[2]), out = le(mod(p[1] * zi), 32);
    out[31] |= Number(mod(p[0] * zi) & N1) << 7;
    return out;
  }

  function le(n, len) {
    var b = new Uint8Array(len);
    for (var i = 0; i < len; i++, n >>= N8) b[i] = Number(n & N255);
    return b;
  }

  function num(b) {
    var n = N0;
    for (var i = b.length - 1; i >= 0; i--) n = (n << N8) | big(b[i]);
    return n;
  }

  function cat() {
    var len = 0, i;
    for (i = 0; i < arguments.length; i++) len += arguments[i].length;
    var out = new Uint8Array(len), at = 0;
    for (i = 0; i < arguments.length; i++) {
      out.set(arguments[i], at);
      at += arguments[i].length;
    }
    return out;
  }

  function utf8(s) {
    return new TextEncoder().encode(s);
  }

  function hex(b) {
    var s = "";
    for (var i = 0; i < b.length; i++) s += (b[i] < 16 ? "0" : "") + b[i].toString(16);
    return s;
  }

  // the key pair for a 32-byte seed
  function keyFrom(seed) {
    curve();
    var h = sha512(seed), a = h.slice(0, 32);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    var s = num(a);
    return { s: s, prefix: h.slice(32), pub: encode(times(s, BASE)) };
  }

  function sign(key, msg) {
    curve();
    var r = mod(num(sha512(cat(key.prefix, msg))), L);
    var big = encode(times(r, BASE));
    var k = mod(num(sha512(cat(big, key.pub, msg))), L);
    return cat(big, le(mod(r + k * key.s, L), 32));
  }

  // slow on purpose: the public key is in the log, so a guessed passphrase costs this much
  var ROUNDS = 65536;

  function seedFrom(call, pass) {
    var h = sha512(utf8("ridgewarden-game player key\n" + call + "\n" + pass));
    for (var i = 0; i < ROUNDS; i++) h = sha512(h);
    return h.slice(0, 32);
  }

  // what the server checks: the payload without psig, plus the event kind, keys sorted
  function signedText(kind, p) {
    var o = {};
    for (var k in p) if (k !== "psig") o[k] = p[k];
    o.kind = kind;
    return "{" + Object.keys(o).sort().map(function (k) {
      return JSON.stringify(k) + ":" + JSON.stringify(o[k]);
    }).join(",") + "}";
  }

  function upper(s) {
    return s.trim().replace(/[a-z]/g, function (c) { return c.toUpperCase(); });
  }

  function fixed(s, places) {
    var x = Number(s);
    return s.trim() !== "" && isFinite(x) ? x.toFixed(places) : s.trim();
  }

  // the payload exactly as the server will log it, tidying the typed fields first
  function payload(form) {
    var els = form.elements, p = {}, i;
    if (els.call) els.call.value = upper(els.call.value);
    if (els.word) els.word.value = upper(els.word.value);
    if (els.text) els.text.value = els.text.value.trim();
    if (els.deg) els.deg.value = fixed(els.deg.value, 1);
    if (els.lat) {
      var at = form.querySelector("[data-at]").value.trim();
      var pos = at ? at.split(/[\s,]+/) : form.querySelector("[data-from]").value.split(",");
      els.lat.value = pos.length === 2 ? fixed(pos[0], 5) : "";
      els.lon.value = pos.length === 2 ? fixed(pos[1], 5) : "";
    }
    for (i = 0; i < els.length; i++) {
      var e = els[i];
      if (e.name && e.name !== "pk" && e.name !== "psig") p[e.name] = e.value;
    }
    p.t = Number(p.t);
    if (p.target) {
      var cut = p.target.indexOf(":");
      p[p.target.slice(0, cut)] = p.target.slice(cut + 1);
      delete p.target;
    }
    return p;
  }

  var MIN_PASS = 12;

  // the key a callsign already plays with, or "" if it has none yet or the site can't say
  function known(call) {
    if (!root.fetch) return Promise.resolve("");
    return root.fetch("/key?call=" + encodeURIComponent(call)).then(function (r) {
      return r.ok ? r.text() : "";
    }, function () {
      return "";
    });
  }

  // where a post can set the callsign's key, the first one asks for the passphrase twice,
  // so a typo isn't kept for good
  function twice(form, call, pass, bound) {
    var again = form.querySelector("[data-again]");
    if (bound || !again) return "";
    if (again.hidden) {
      again.hidden = false;
      again.querySelector("input").focus();
      return "This post sets " + call + "'s key: type the passphrase again to check it.";
    }
    var other = again.querySelector("input").value.normalize("NFKC");
    return other === pass ? "" : "The two passphrases don't match.";
  }

  function submit(ev) {
    var form = ev.target;
    if (form.dataset.ready) return;
    ev.preventDefault();
    var note = form.querySelector(".signing"), button = form.querySelector("button[type=submit]");
    function fail(why) {
      note.textContent = why;
      button.disabled = false;
    }
    if (!big || !root.TextEncoder || !"".normalize || !Array.from) {
      return fail("This browser is too old to sign posts. Try a newer one.");
    }
    // the same passphrase typed on another device or keyboard makes the same key
    var pass = form.querySelector("[data-pass]").value.normalize("NFKC");
    if (Array.from(pass).length < MIN_PASS) {
      return fail("Use a passphrase of at least " + MIN_PASS + " characters.");
    }
    var p = payload(form);
    button.disabled = true;
    known(p.call).then(function (bound) {
      var why = twice(form, p.call, pass, bound);
      if (why) return fail(why);
      note.textContent = "Signing with your key...";
      // give the page a moment to show that before the slow part
      setTimeout(function () {
        try {
          var key = keyFrom(seedFrom(p.call, pass));
          p.pk = hex(key.pub);
          if (bound && bound !== p.pk) {
            return fail("That passphrase doesn't make the key " + p.call + " plays with. Use the one you used before.");
          }
          form.elements.pk.value = p.pk;
          form.elements.psig.value = hex(sign(key, utf8(signedText(form.dataset.sign, p))));
          form.dataset.ready = "1";
          form.submit();
        } catch (e) {
          fail("Couldn't sign the post: " + e.message);
        }
      }, 30);
    });
  }

  // a whole number below n from the browser's random source, without modulo bias
  function below(n) {
    var r = new Uint32Array(1), top = 4294967296 - 4294967296 % n;
    do root.crypto.getRandomValues(r); while (r[0] >= top);
    return r[0] % n;
  }

  // four words from the page's list, each with two digits: about 54 bits
  function suggest(form, button) {
    var words = button.dataset.words.split(" "), out = [];
    for (var i = 0; i < 4; i++) out.push(words[below(words.length)] + String(100 + below(100)).slice(1));
    var phrase = out.join(" "), again = form.querySelector("[data-again] input");
    form.querySelector("[data-pass]").value = phrase;
    if (again) again.value = phrase;
    button.nextElementSibling.textContent = "Your passphrase: " + phrase + ". Write it down; every post needs it.";
  }

  root.rwg = { sha512: sha512, keyFrom: keyFrom, sign: sign, seedFrom: seedFrom, signedText: signedText, hex: hex, utf8: utf8 };

  if (root.document) {
    var forms = root.document.querySelectorAll("form[data-sign]");
    for (var i = 0; i < forms.length; i++) {
      forms[i].addEventListener("submit", submit);
      var offer = forms[i].querySelector("[data-suggest]");
      if (offer && root.crypto && root.crypto.getRandomValues) {
        offer.hidden = false;
        var b = offer.querySelector("button");
        b.addEventListener("click", suggest.bind(null, forms[i], b));
      }
    }
    // a page brought back with the back button would otherwise keep its old signature
    root.addEventListener("pageshow", function () {
      for (var j = 0; j < forms.length; j++) {
        delete forms[j].dataset.ready;
        forms[j].querySelector("button[type=submit]").disabled = false;
      }
    });
  }
})(this);
