# ADR-0002: 微信扫码登录按"不触发 reauth"处理

> 2026-10-31：本决策已被远端安全修复 `1c4f57d` 取代。微信扫码后若落入 reauth 页，
> `wechat_login()` 返回 `ReauthRequired`，不再仅记录 warning 后返回成功。以下保留原决策背景。

2026-08-07 决定：`wechat_login()`（登录页扫码，`combinedLogin.do?type=weixin` 无 `reAuth`
参数）保持既有成功判定，不因扫码后可能落回 reauth 页而报错——仅在该场景实际发生时记
warning 日志兜底。

分析仓库只实测过"reauth 页内的微信路径"（`reAuth=2`），登录页扫码完成后 TGT 是否被锁
**没有实测记录**。曾考虑让 `wechat_login()` 返回 `ReauthRequired` 统一编排，但用户选择
按"扫码不触发 reauth"处理（扫码本身是强因素，服务端大概率直接放行）。若假设错误，症状
为"扫码登录返回成功但业务请求失败"，可诊断（warning 日志 + 会话检查），代价可控。
