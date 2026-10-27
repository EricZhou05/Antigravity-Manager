import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

import fs from "fs";
import path from "path";
import os from "os";
import cp from "child_process";
import type { Plugin } from "vite";

function dshApiPlugin(): Plugin {
  return {
    name: "dsh-api-plugin",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url?.startsWith("/api/proxy/dsh/")) {
          return next();
        }

        const readBody = () => new Promise<any>((resolve) => {
          let data = "";
          req.on("data", chunk => data += chunk);
          req.on("end", () => {
            try {
              resolve(data ? JSON.parse(data) : {});
            } catch {
              resolve({});
            }
          });
        });

        const body = await readBody();
        const home = os.homedir();
        const dshDir = path.join(home, ".dsh");

        if (req.url === "/api/proxy/dsh/status") {
          const desktopPatch = path.join(dshDir, "profiles", "desktop", "cordis.patch.yml");
          const creds = path.join(dshDir, ".credentials.yaml");

          let installed = fs.existsSync(dshDir) || fs.existsSync("/Applications/DeepSeek Harness.app");
          let version: string | null = "0.2.1";

          try {
            const out = cp.execSync(
              '"/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh" --version 2>/dev/null || dsh --version',
              { encoding: "utf8" }
            );
            const m = out.match(/(\d+\.\d+(?:\.\d+)?)/);
            if (m) version = m[1];
          } catch {}

          let is_synced = false;
          let has_backup = fs.existsSync(`${desktopPatch}.antigravity.bak`) || fs.existsSync(`${creds}.antigravity.bak`);
          let current_base_url: string | null = null;
          let current_model: string | null = null;

          if (fs.existsSync(desktopPatch)) {
            const content = fs.readFileSync(desktopPatch, "utf8");
            const mUrl = content.match(/^\s*baseURL:\s*['"]?([^'"\s#]+)['"]?/m);
            if (mUrl) current_base_url = mUrl[1];
            const mMod = content.match(/id:\s*agent-default-model[\s\S]*?config:[\s\S]*?model:\s*['"]?([a-zA-Z0-9_.-]+)['"]?/);
            if (mMod) current_model = mMod[1];

            const normProxy = (body.proxyUrl || "").replace(/\/+$/, "").replace(/\/v1$/, "");
            const normCur = (current_base_url || "").replace(/\/+$/, "").replace(/\/v1$/, "");
            if (normCur && normProxy && normCur === normProxy && content.includes("provider: antigravity")) {
              is_synced = true;
            }
          }

          res.setHeader("Content-Type", "application/json");
          res.end(JSON.stringify({
            installed,
            version,
            is_synced,
            has_backup,
            current_base_url,
            current_model,
            files: [
              "profiles/desktop/cordis.patch.yml",
              "profiles/web/cordis.patch.yml",
              ".credentials.yaml"
            ]
          }));
          return;
        }

        if (req.url === "/api/proxy/dsh/sync") {
          const proxyUrl = body.proxyUrl || "http://127.0.0.1:8045/v1";
          const apiKey = body.apiKey || "";
          const targetModel = body.model || "gemini-3.8-flash";
          const normalizedProxyUrl = proxyUrl.replace(/\/+$/, "").endsWith("/v1")
            ? proxyUrl.replace(/\/+$/, "")
            : `${proxyUrl.replace(/\/+$/, "")}/v1`;

          // 1. Credentials
          fs.mkdirSync(dshDir, { recursive: true });
          const credsPath = path.join(dshDir, ".credentials.yaml");
          let creds = fs.existsSync(credsPath) ? fs.readFileSync(credsPath, "utf8") : "";
          if (fs.existsSync(credsPath) && !fs.existsSync(`${credsPath}.antigravity.bak`)) {
            fs.copyFileSync(credsPath, `${credsPath}.antigravity.bak`);
          }
          if (!creds.trim()) {
            creds = `version: 1\nrefs:\n  ANTIGRAVITY_API_KEY: ${apiKey}\n`;
          } else if (/(ANTIGRAVITY_API_KEY:\s*)[^\r\n]+/.test(creds)) {
            creds = creds.replace(/(ANTIGRAVITY_API_KEY:\s*)[^\r\n]+/, `$1${apiKey}`);
          } else if (/(refs:\s*\{[\s\S]*?)(\})/m.test(creds)) {
            creds = creds.replace(/(refs:\s*\{[\s\S]*?)(\})/m, (match, p1) => {
              const trimmed = p1.trimEnd();
              const needsComma = !trimmed.endsWith("{") && !trimmed.endsWith(",");
              return `${trimmed}${needsComma ? "," : ""}\n    ANTIGRAVITY_API_KEY: ${apiKey}\n  }`;
            });
          } else if (/(^refs:\s*)$/m.test(creds)) {
            creds = creds.replace(/(^refs:\s*)$/m, `$1\n  ANTIGRAVITY_API_KEY: ${apiKey}`);
          } else {
            creds = `${creds.trimEnd()}\nrefs:\n  ANTIGRAVITY_API_KEY: ${apiKey}\n`;
          }
          fs.writeFileSync(credsPath, creds, "utf8");

          // 2. Models block
          const models = [
            { id: "gemini-3.8-flash", name: "Gemini 3.8 Flash", contextWindow: 1048576, maxTokens: 128000, reasoning: true },
            { id: "gemini-3.8-flash-high", name: "Gemini 3.8 Flash (High)", contextWindow: 1048576, maxTokens: 128000, reasoning: true },
            { id: "gemini-3.1-pro-high", name: "Gemini 3.1 Pro (High)", contextWindow: 1048576, maxTokens: 128000, reasoning: true },
            { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6", contextWindow: 256000, maxTokens: 128000, reasoning: false },
            { id: "claude-opus-4-6-thinking", name: "Claude Opus 4.6 (Thinking)", contextWindow: 256000, maxTokens: 128000, reasoning: false },
            { id: "claude-opus-5-5-high", name: "Claude Opus 5.5 (High)", contextWindow: 1000000, maxTokens: 128000, reasoning: false },
            { id: "gpt-oss-120b-medium", name: "GPT-OSS 120B (Medium)", contextWindow: 131072, maxTokens: 32000, reasoning: false }
          ];
          if (!models.some(m => m.id === targetModel)) {
            models.unshift({ id: targetModel, name: targetModel, contextWindow: 1000000, maxTokens: 128000, reasoning: false });
          }

          let modelsYaml = "";
          for (const m of models) {
            modelsYaml += `          - id: ${m.id}\n            name: ${m.name}\n            contextWindow: ${m.contextWindow}\n            input:\n              - text\n              - image\n            maxTokens: ${m.maxTokens}\n`;
            if (m.reasoning) {
              modelsYaml += "            reasoningEfforts:\n              off: null\n              low: low\n              medium: medium\n              high: high\n";
            }
          }

          const providerBlock = `      antigravity:\n        displayName: antigravity\n        apiKeyEnv: ANTIGRAVITY_API_KEY\n        api: openai-completions\n        baseURL: ${normalizedProxyUrl}\n        models:\n${modelsYaml}`;
          const defaultModelBlock = `- id: agent-default-model\n  name: "@deepseek-ai/dsh-agent-default-model"\n  config:\n    provider: antigravity\n    model: ${targetModel}\n`;

          function updatePatch(content: string) {
            const trimmed = content.trim();
            if (!trimmed || trimmed === "[]" || !trimmed.includes("- id:")) {
              const comment = "# Your patch layer for this dsh profile, applied after every bundle layer:\n# a top-level YAML array of loader patch entries (id-targeted config\n# overrides, disables, and insert lists; `!!js` expressions allowed).\n";
              return `${comment}${defaultModelBlock}- id: llm-pi-ai\n  name: "@deepseek-ai/dsh-llm-pi-ai"\n  config:\n    providers:\n${providerBlock}\n`;
            }
            let result = content;
            const reAgent = /(- id:\s*agent-default-model[\s\S]*?config:\s*\n)([\s\S]*?)(?=\n- id:|\z)/;
            if (reAgent.test(result)) {
              result = result.replace(reAgent, (match, prefix, body) => {
                let newBody = body;
                if (/provider:\s*[^\r\n]+/.test(newBody)) newBody = newBody.replace(/provider:\s*[^\r\n]+/, "provider: antigravity");
                else newBody = `    provider: antigravity\n${newBody}`;
                if (/model:\s*[^\r\n]+/.test(newBody)) newBody = newBody.replace(/model:\s*[^\r\n]+/, `model: ${targetModel}`);
                else newBody = `${newBody}\n    model: ${targetModel}`;
                return `${prefix}${newBody}`;
              });
            } else {
              result = `${result.trimEnd()}\n${defaultModelBlock}`;
            }

            const reLlm = /(- id:\s*llm-pi-ai[\s\S]*?config:\s*\n\s*providers:\s*\n)([\s\S]*?)(?=\n- id:|\z)/;
            if (reLlm.test(result)) {
              result = result.replace(reLlm, (match, prefix, providersBody) => {
                const reAg = /( {6}antigravity:\s*\n)([\s\S]*?)(?=\n {6}[a-zA-Z0-9_-]+:|\z)/;
                if (reAg.test(providersBody)) return `${prefix}${providersBody.replace(reAg, `${providerBlock}\n`)}`;
                else return `${prefix}${providerBlock}\n${providersBody}`;
              });
            } else {
              result = `${result.trimEnd()}\n- id: llm-pi-ai\n  name: "@deepseek-ai/dsh-llm-pi-ai"\n  config:\n    providers:\n${providerBlock}\n`;
            }
            return result;
          }

          // Desktop
          const desktopDir = path.join(dshDir, "profiles", "desktop");
          fs.mkdirSync(desktopDir, { recursive: true });
          const desktopPath = path.join(desktopDir, "cordis.patch.yml");
          let desktopContent = fs.existsSync(desktopPath) ? fs.readFileSync(desktopPath, "utf8") : "";
          if (fs.existsSync(desktopPath) && !fs.existsSync(`${desktopPath}.antigravity.bak`)) {
            fs.copyFileSync(desktopPath, `${desktopPath}.antigravity.bak`);
          }
          fs.writeFileSync(desktopPath, updatePatch(desktopContent), "utf8");

          // Web
          const webDir = path.join(dshDir, "profiles", "web");
          fs.mkdirSync(webDir, { recursive: true });
          const webPath = path.join(webDir, "cordis.patch.yml");
          let webContent = fs.existsSync(webPath) ? fs.readFileSync(webPath, "utf8") : "";
          if (fs.existsSync(webPath) && !fs.existsSync(`${webPath}.antigravity.bak`)) {
            fs.copyFileSync(webPath, `${webPath}.antigravity.bak`);
          }
          fs.writeFileSync(webPath, updatePatch(webContent), "utf8");

          res.setHeader("Content-Type", "application/json");
          res.end(JSON.stringify({ success: true }));
          return;
        }

        if (req.url === "/api/proxy/dsh/restore") {
          const files = [
            path.join(dshDir, "profiles", "desktop", "cordis.patch.yml"),
            path.join(dshDir, "profiles", "web", "cordis.patch.yml"),
            path.join(dshDir, ".credentials.yaml")
          ];
          for (const f of files) {
            if (fs.existsSync(`${f}.antigravity.bak`)) {
              fs.copyFileSync(`${f}.antigravity.bak`, f);
            }
          }
          res.setHeader("Content-Type", "application/json");
          res.end(JSON.stringify({ success: true }));
          return;
        }

        if (req.url === "/api/proxy/dsh/config") {
          let targetPath = path.join(dshDir, "profiles", "desktop", "cordis.patch.yml");
          if (body.fileName === "profiles/web/cordis.patch.yml") {
            targetPath = path.join(dshDir, "profiles", "web", "cordis.patch.yml");
          } else if (body.fileName === ".credentials.yaml") {
            targetPath = path.join(dshDir, ".credentials.yaml");
          }
          const content = fs.existsSync(targetPath) ? fs.readFileSync(targetPath, "utf8") : "";
          res.setHeader("Content-Type", "application/json");
          res.end(JSON.stringify(content));
          return;
        }

        next();
      });
    }
  };
}


// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), dshApiPlugin()],
  base: "./",

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  resolve: {
    alias: {
      "@lobehub/ui/base-ui": "antd",
    },
  },
  build: {
    rollupOptions: {
      external: ["@lobehub/ui/base-ui"],
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
        protocol: "ws",
        host,
        port: 1421,
      }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
    proxy: {
      "/api/": {
        target: "http://127.0.0.1:8045",
        changeOrigin: true,
      },
    },
  },
}));
