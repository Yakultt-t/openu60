import crypto from "node:crypto";
const GW = "192.168.0.1";
const ANON = "00000000000000000000000000000000";
const agentPw = process.env.AGENT_PW;
const routerPw = process.env.ROUTER_PW;
function sha256Hex(x) { return crypto.createHash("sha256").update(x, "utf8").digest("hex").toUpperCase(); }
const info = await (async () => { const r = await ubus(ANON, "zwrt_web", "web_login_info", { "": "" }); return r?.[0]?.result?.[1] || {}; })();
const salt = info.zte_web_sault || "";
const loginRes = await ubus(ANON, "zwrt_web", "web_login", { password: sha256Hex(sha256Hex(routerPw) + salt) });
const SESSION = loginRes?.[0]?.result?.[1]?.ubus_rpc_session;
console.log("router web login:", SESSION ? "ok" : JSON.stringify(loginRes).slice(0, 160));
if (!SESSION) process.exit(1);
async function ubus(session, obj, method, params) {
  const r = await fetch(`http://${GW}/ubus/?t=${Date.now()}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Referer: `http://${GW}/`, Origin: `http://${GW}`, "X-Requested-With": "XMLHttpRequest" },
    body: JSON.stringify([{ jsonrpc: "2.0", id: 1, method: "call", params: [session, obj, method, params] }]),
  });
  return r.json();
}
// 1. certificate
const crt = await ubus(SESSION, "zwrt_web", "web_crt_get", {});
const pem = crt?.[0]?.result?.[1]?.result;
console.log("crt_get ok:", !!pem, pem ? pem.slice(0, 40).replace(/\n/g, " ") : JSON.stringify(crt).slice(0, 200));
if (!pem) process.exit(1);
let pub;
try { pub = crypto.createPublicKey(pem); } catch (e) { pub = new crypto.X509Certificate(pem).publicKey; }
console.log("public key:", pub.asymmetricKeyType, pub.asymmetricKeyDetails?.modulusLength);
// 2. random 32-byte key, RSA PKCS#1 v1.5, base64 (JSEncrypt output is base64)
const key = crypto.randomBytes(32);
const enc = crypto.publicEncrypt({ key: pub, padding: crypto.constants.RSA_PKCS1_PADDING }, Buffer.from(key.toString("hex")));
const setr = await ubus(SESSION, "zwrt_web", "web_http_enstr_set", { web_enstr: enc.toString("base64") });
console.log("enstr_set:", JSON.stringify(setr).slice(0, 200));
// 3. agent sms list
const login = await (await fetch("http://192.168.0.1:9090/api/auth/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password: agentPw }) })).json();
const token = login.data.token;
const list = await (await fetch("http://192.168.0.1:9090/api/sms/list", { method: "POST", headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` }, body: JSON.stringify({ page: 0, data_per_page: 500, mem_store: 2, tags: 10, order_by: "order by id desc" }) })).json();
function dec(b64) {
  const buf = Buffer.from(b64, "base64");
  if (buf.length <= 28) return null;
  const iv = buf.subarray(0, 12), tag = buf.subarray(12, 28), ct = buf.subarray(28);
  const d = crypto.createDecipheriv("aes-256-gcm", key, iv); d.setAuthTag(tag);
  try { return Buffer.concat([d.update(ct), d.final()]).toString("utf8"); } catch (e) { return "DECRYPT_FAIL:" + e.message; }
}
for (const m of list.data.messages) {
  const num = dec(m.number), content = dec(m.content);
  const maskedNum = num ? num.replace(/\d(?=\d{3})/g, "*") : num;
  // content is UCS-2 hex after decryption (per the web UI decodeMessage)
  let text = content;
  if (content && /^[0-9A-Fa-f]+$/.test(content) && content.length % 4 === 0) {
    const u = []; for (let i = 0; i < content.length; i += 4) u.push(parseInt(content.slice(i, i + 4), 16));
    text = String.fromCharCode(...u);
  }
  console.log(`id=${m.id} tag=${m.tag} number=${maskedNum} text=${JSON.stringify(text).slice(0, 120)}`);
}
