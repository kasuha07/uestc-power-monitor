# ADR-0001: 错误驱动的 reauth API（ReauthRequired + 三步方法）

2026-08-07 决定：`login()` 遇到多因子策略锁定（TGT 已发但不可换 ST）时返回
`UestcClientError::ReauthRequired { context: ReauthContext }`，并提供
`change_reauth_type` / `send_reauth_code` / `submit_reauth` 三步方法完成认证。

此前 `login()` 只检查最终 URL 是否落在 `/authserver/login`，reauth 302 会被误判为
"登录成功"——TGT 被锁但调用方毫不知情，业务请求全部失败。曾考虑回调驱动
（`login_with_reauth(handler)`），但 async 闭包签名复杂、错误处理与"等待用户输入验证码"
的间隙难以表达；错误驱动让调用方（如 power-monitor 的 TTY 交互层）完全掌控节奏，
async/blocking 双实现对称。库自身不做 TTY 交互（微信终端二维码除外，沿用既有行为）。
