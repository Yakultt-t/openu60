//<script>
// SIM 随行 / OpenU60 安装与更新 —— UFI-TOOLS 插件（中兴 U60 Pro）
// 在路由器上下载 OpenU60 的签名发布包（https://github.com/Yakultt-t/openu60），核对签名和
// 每个文件的校验值后安装；也能更新、卸载。需要 UFI-TOOLS 的「高级功能」（root 权限）。
// 装好后用 iPhone App「SIM 随行」配对；以后在 App 里就能在线更新，不再需要这个插件。
(() => {
    const BASE = "https://github.com/Yakultt-t/openu60/releases/download/router-update"
    const SCRIPT = "/tmp/openu60-netinstall.sh"
    const LOG = "/tmp/openu60-netinstall.log"
    const PATHS = "export PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH;"
    const FETCH = `curl -fsSL -m 60 -o ${SCRIPT} ${BASE}/openu60-netinstall.sh`
    const sh = (cmd, timeout = 30000) => runShellWithRoot(`${PATHS} ${cmd}`, timeout)
    const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]))
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
    let busy = false

    const isRoot = async () => {
        const r = await sh("id -u")
        return r.success && String(r.content.content ?? r.content).trim().endsWith("0")
    }
    const text = (r) => String(r?.content?.content ?? r?.content ?? "")

    const parseStatus = (out) => {
        const s = {}
        out.split("\n").forEach((l) => { const i = l.indexOf("="); if (i > 0) s[l.slice(0, i).trim()] = l.slice(i + 1).trim() })
        return s
    }

    const render = (s) => {
        const box = document.querySelector("#openu60_status")
        if (!box) return
        if (!s) { box.innerHTML = "读取中…"; return }
        const latest = (s.latest || "").trim().split(/\s+/)
        const latestLabel = latest[0] ? `${esc(latest[0])}（${esc(latest[1] || "")}）` : "读取失败（路由器连不上 GitHub？）"
        let installed = "未安装"
        if (s.installed === "yes") {
            const v = s.version ? `${esc(s.version)}（${esc(s.build || "0")}）` : "已安装"
            installed = `${v} · ${s.running === "yes" ? "运行中" : "没有运行"}${s.channel === "dev" ? " · 自己编译的开发版" : ""}`
        }
        box.innerHTML = `
            <div>设备：${esc(s.model || "?")} · ${esc(s.firmware || "?")}</div>
            <div>OpenU60：${installed}</div>
            <div>最新版本：${latestLabel}</div>
            <div>触摸屏程序：${s.screen === "yes" ? "已安装（连按三下电源键切换）" : "未安装"}</div>`
        const btn = document.querySelector("#openu60_install_btn")
        if (btn) btn.textContent = s.installed === "yes" ? "重新安装" : "安装"
        const ota = document.querySelector("#openu60_ota_btn")
        if (ota) {
            ota.style.display = s.ota === "yes" ? "" : "none"
            ota.textContent = `在线更新到 ${latest[0] || "新版本"}`
        }
        const tip = document.querySelector("#openu60_ota_tip")
        if (tip) tip.style.display = s.installed === "yes" && s.ota !== "yes" && latest[1] && Number(latest[1]) > Number(s.build || 0) ? "" : "none"
    }

    const refresh = async () => {
        render(null)
        if (!(await isRoot())) {
            document.querySelector("#openu60_status").innerHTML = "需要 root：请先在 UFI-TOOLS 里打开「高级功能」，等 1–2 分钟后再刷新。"
            return
        }
        const r = await sh(`${FETCH} && sh ${SCRIPT} status`, 40000)
        if (!r.success) {
            document.querySelector("#openu60_status").innerHTML = `读取失败：${esc(text(r))}`
            return
        }
        render(parseStatus(text(r)))
    }

    const showPassword = (pw) => {
        const { el, close } = createFixedToast("openu60_pw_toast", `
            <div style="pointer-events:all;min-width:280px;text-align:center;line-height:1.7">
                <div style="font-weight:bold;margin-bottom:6px">SIM 随行密码（只显示这一次）</div>
                <div style="font-family:monospace;font-size:20px;letter-spacing:1px;user-select:all">${esc(pw)}</div>
                <div style="font-size:12px;opacity:.8;margin:6px 0">请抄下来保存。iPhone 打开「SIM 随行」，连上路由器的 Wi-Fi，输入这个密码完成配对。</div>
                <button class="btn" id="openu60_pw_copy">复制</button>
                <button class="btn" id="openu60_pw_close">我已保存</button>
            </div>`)
        el.querySelector("#openu60_pw_copy").onclick = async () => {
            try { await navigator.clipboard.writeText(pw); createToast("已复制") } catch { createToast("复制失败，请手动抄写", "red") }
        }
        el.querySelector("#openu60_pw_close").onclick = () => close()
    }

    // Runs the router script in the background (UFI-TOOLS caps a command at 100 s) and shows
    // its log until it finishes.
    const runJob = async (args, title) => {
        if (busy) return
        if (!(await isRoot())) return createToast("没有 root：请先打开「高级功能」", "red")
        busy = true
        const start = await sh(`${FETCH} && rm -f ${LOG} && (nohup sh ${SCRIPT} ${args} > ${LOG} 2>&1 &) && echo started`, 70000)
        if (!start.success || !text(start).includes("started")) {
            busy = false
            return createToast(`启动失败：${text(start)}`, "red")
        }
        const { el, close } = createFixedToast("openu60_job_toast", `<pre style="white-space:pre-wrap;min-width:300px;text-align:left">${esc(title)}…</pre>`)
        const t0 = Date.now()
        let password = ""
        let done = false
        while (Date.now() - t0 < 10 * 60 * 1000) {
            await sleep(1500)
            const r = await sh(`cat ${LOG} 2>/dev/null`)
            const log = text(r)
            const pw = log.match(/@@PASSWORD (\S+)/)
            if (pw) password = pw[1]
            const lines = log.split("\n")
                .filter((l) => !l.startsWith("@@PASSWORD"))
                .map((l) => l.replace(/^@@STEP (\S+) /, "[$1] ").replace(/^@@OK /, "✓ ").replace(/^@@FAIL /, "✗ ").replace(/^@@DONE.*/, "完成"))
            el.style.maxHeight = "420px"
            el.style.overflow = "auto"
            el.innerHTML = `<pre style="pointer-events:all;white-space:pre-wrap;min-width:300px;text-align:left">${esc(title)}\n\n${esc(lines.slice(-14).join("\n"))}</pre>`
            el.scrollTo({ top: 99999 })
            if (log.includes("@@DONE") || log.includes("@@FAIL")) {
                done = true
                createToast(log.includes("@@FAIL") ? "没有完成，请看日志" : "完成", log.includes("@@FAIL") ? "red" : "")
                break
            }
        }
        // The log may hold the password: remove it from the router once read.
        await sh(`rm -f ${LOG} ${SCRIPT}`)
        setTimeout(close, done ? 6000 : 0)
        busy = false
        if (password) showPassword(password)
        refresh()
    }

    const install = async () => {
        const channel = document.querySelector("input[name=openu60_channel]:checked")?.value || "relay"
        const noscreen = document.querySelector("#openu60_noscreen")?.checked ? " noscreen" : ""
        if (!confirm("安装或更新 OpenU60？\n\n会关闭中兴固件自动升级（升级会把 OpenU60 冲掉），路由器上的短信和电话由 OpenU60 接管。已有的 SIM 随行密码会保留。")) return
        runJob(`install ${channel}${noscreen}`, "安装 OpenU60")
    }

    // OTA: the agent updates itself (signed, verified, rolls back by itself if the new
    // version does not start); the password and pairing stay as they are.
    const ota = async () => {
        if (!confirm("在线更新 OpenU60？\n\n路由器会自己下载新版本（约 9 MB）、核对签名后安装并重启软件，期间电话和短信暂停约一分钟；新版本启动不了会自动恢复旧版本。")) return
        runJob("ota", "在线更新 OpenU60")
    }

    const uninstall = async () => {
        if (!confirm("卸载 OpenU60？\n\n路由器会恢复原厂屏幕和热点模式；/data/openu60 里的数据保留。")) return
        runJob("uninstall", "卸载 OpenU60")
    }

    const mount = async () => {
        while (!document.querySelector(".functions-container") || typeof UFI_DATA === "undefined" || UFI_DATA.loginfo != "ok") await sleep(300)
        document.querySelector(".functions-container").insertAdjacentHTML("afterend", `
<div id="OPENU60_KANO" style="width:100%;margin-top:10px;">
    <div class="title" style="margin:6px 0;">
        <strong>SIM 随行 / OpenU60</strong>
        <div style="display:inline-block;" id="collapse_openu60_btn"></div>
    </div>
    <div class="collapse" id="collapse_openu60" data-name="close" style="height:0px;overflow:hidden;">
        <div class="collapse_box">
            <ul class="deviceList"><li style="padding:10px;line-height:1.8">
                <div id="openu60_status">读取中…</div>
                <div style="margin-top:8px">
                    <label><input type="radio" name="openu60_channel" value="relay" checked> 带作者的中继（在外面也能用手机连路由器）</label><br>
                    <label><input type="radio" name="openu60_channel" value="public"> 不带中继（只在家用，或自建中继）</label><br>
                    <label><input type="checkbox" id="openu60_noscreen"> 不装触摸屏程序（已经用了别的屏幕插件时勾选）</label>
                </div>
                <div style="margin:8px 0;display:flex;gap:10px;flex-wrap:wrap" id="openu60_actions"></div>
                <div id="openu60_ota_tip" style="display:none;font-size:12px;color:#e8a33c">有新版本，但这台路由器上的版本还不支持在线更新：点「重新安装」更新一次（保留密码），以后就能在线更新。</div>
                <div style="font-size:12px;opacity:.75">装好后用 iPhone App「SIM 随行」配对；以后在 App「设置 → 设备 → 路由器软件」里在线更新。
                说明和源码：<a href="https://github.com/Yakultt-t/openu60" target="_blank">github.com/Yakultt-t/openu60</a></div>
            </li></ul>
        </div>
    </div>
</div>`)
        const box = document.querySelector("#openu60_actions")
        const mk = (label, fn, id) => {
            const b = document.createElement("button")
            b.classList.add("btn")
            b.textContent = label
            if (id) b.id = id
            b.onclick = fn
            box.appendChild(b)
        }
        mk("在线更新", ota, "openu60_ota_btn")
        document.querySelector("#openu60_ota_btn").style.display = "none"
        mk("安装", install, "openu60_install_btn")
        mk("刷新状态", refresh)
        mk("卸载", uninstall)
        collapseGen("#collapse_openu60_btn", "#collapse_openu60", "#collapse_openu60", () => { })
        refresh()
    }
    mount()
})()
//</script>
