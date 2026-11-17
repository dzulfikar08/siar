import { createHash, randomBytes } from "node:crypto";

// BIP-340 Schnorr signatures over secp256k1 with BigInt, for signing Nostr events.
// Not constant-time: fine for signing your own posts on your own server.

const P = 0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2fn;
const N = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141n;
const G: Pt = [0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798n, 0x483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8n];
type Pt = [bigint, bigint] | null;

const mod = (a: bigint, m = P) => ((a % m) + m) % m;
function inv(a: bigint, m = P): bigint {
  let [lo, hi, x0, x1] = [mod(a, m), m, 1n, 0n];
  while (lo > 1n) {
    const q = hi / lo;
    [lo, hi] = [hi - q * lo, lo];
    [x0, x1] = [x1 - q * x0, x0];
  }
  return mod(x0, m);
}

function add(a: Pt, b: Pt): Pt {
  if (!a) return b;
  if (!b) return a;
  const [x1, y1] = a;
  const [x2, y2] = b;
  if (x1 === x2 && mod(y1 + y2) === 0n) return null;
  const l = x1 === x2 ? mod(3n * x1 * x1 * inv(2n * y1)) : mod((y2 - y1) * inv(x2 - x1));
  const x3 = mod(l * l - x1 - x2);
  return [x3, mod(l * (x1 - x3) - y1)];
}

function mul(k: bigint, pt: Pt = G): Pt {
  let r: Pt = null;
  let q = pt;
  while (k > 0n) {
    if (k & 1n) r = add(r, q);
    q = add(q, q);
    k >>= 1n;
  }
  return r;
}

const toHex = (n: bigint) => n.toString(16).padStart(64, "0");
const bytes = (n: bigint) => Buffer.from(toHex(n), "hex");
const num = (b: Uint8Array) => BigInt(`0x${Buffer.from(b).toString("hex")}`);
const sha = (...parts: Uint8Array[]) => {
  const h = createHash("sha256");
  for (const p of parts) h.update(p);
  return h.digest();
};
const tagged = (tag: string, ...parts: Uint8Array[]) => {
  const t = sha(Buffer.from(tag));
  return sha(t, t, ...parts);
};

export function getPublicKey(privHex: string): string {
  const pt = mul(BigInt(`0x${privHex}`))!;
  return toHex(pt[0]);
}

export function sign(msg: Uint8Array, privHex: string): string {
  const d0 = BigInt(`0x${privHex}`);
  if (d0 <= 0n || d0 >= N) throw new Error("Invalid private key");
  const Pp = mul(d0)!;
  const d = Pp[1] % 2n === 0n ? d0 : N - d0;
  const aux = randomBytes(32);
  const t = bytes(d ^ num(tagged("BIP0340/aux", aux)));
  const k0 = mod(num(tagged("BIP0340/nonce", t, bytes(Pp[0]), msg)), N);
  if (k0 === 0n) throw new Error("Bad nonce");
  const R = mul(k0)!;
  const k = R[1] % 2n === 0n ? k0 : N - k0;
  const e = mod(num(tagged("BIP0340/challenge", bytes(R[0]), bytes(Pp[0]), msg)), N);
  return toHex(R[0]) + toHex(mod(k + e * d, N));
}

export function verify(sigHex: string, msg: Uint8Array, pubHex: string): boolean {
  const x = BigInt(`0x${pubHex}`);
  const y2 = mod(x ** 3n + 7n);
  let y = (() => {
    // sqrt mod p, p ≡ 3 mod 4
    let r = 1n;
    let b = y2;
    let e = (P + 1n) / 4n;
    while (e > 0n) {
      if (e & 1n) r = mod(r * b);
      b = mod(b * b);
      e >>= 1n;
    }
    return r;
  })();
  if (mod(y * y) !== y2) return false;
  if (y % 2n) y = P - y;
  const r = BigInt(`0x${sigHex.slice(0, 64)}`);
  const s = BigInt(`0x${sigHex.slice(64)}`);
  const e = mod(num(tagged("BIP0340/challenge", bytes(r), bytes(x), msg)), N);
  const R = add(mul(s), mul(N - e, [x, y]));
  return !!R && R[1] % 2n === 0n && R[0] === r;
}

// ---- bech32 (NIP-19 nsec/npub/note) ----
const CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
function polymod(values: number[]) {
  const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
  let chk = 1;
  for (const v of values) {
    const b = chk >> 25;
    chk = ((chk & 0x1ffffff) << 5) ^ v;
    for (let i = 0; i < 5; i++) if ((b >> i) & 1) chk ^= GEN[i]!;
  }
  return chk;
}
const hrpExpand = (h: string) => [...[...h].map((c) => c.charCodeAt(0) >> 5), 0, ...[...h].map((c) => c.charCodeAt(0) & 31)];
function convert(data: number[], from: number, to: number, pad: boolean) {
  let acc = 0;
  let bits = 0;
  const out: number[] = [];
  for (const v of data) {
    acc = (acc << from) | v;
    bits += from;
    while (bits >= to) {
      bits -= to;
      out.push((acc >> bits) & ((1 << to) - 1));
    }
  }
  if (pad && bits) out.push((acc << (to - bits)) & ((1 << to) - 1));
  return out;
}
export function bech32Decode(s: string): { hrp: string; hex: string } {
  s = s.toLowerCase();
  const pos = s.lastIndexOf("1");
  const hrp = s.slice(0, pos);
  const data = [...s.slice(pos + 1)].map((c) => CHARSET.indexOf(c));
  if (data.includes(-1) || polymod([...hrpExpand(hrp), ...data]) !== 1) throw new Error("Invalid bech32 string");
  return { hrp, hex: Buffer.from(convert(data.slice(0, -6), 5, 8, false)).toString("hex") };
}
export function bech32Encode(hrp: string, hex: string): string {
  const data = convert([...Buffer.from(hex, "hex")], 8, 5, true);
  const mod = polymod([...hrpExpand(hrp), ...data, 0, 0, 0, 0, 0, 0]) ^ 1;
  const chk = Array.from({ length: 6 }, (_, i) => (mod >> (5 * (5 - i))) & 31);
  return `${hrp}1${[...data, ...chk].map((d) => CHARSET[d]).join("")}`;
}
