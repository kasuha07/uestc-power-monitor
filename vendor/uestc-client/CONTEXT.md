# UESTC 统一身份认证（idas CAS）上下文

uestc-client 是对 idas.uestc.edu.cn（Wisedu 定制 Apereo CAS 5.x）登录协议的封装。
本词汇表定义该协议领域内本仓库使用的术语，协议细节以 `uestc-idas-analysis` 工作区
的 wire 实测为准。

## 认证会话

**TGT（Ticket Granting Ticket）**:
登录成功后服务端发放的会话票据，以 `CASTGC` cookie 形式存在，HttpOnly、`Path=/authserver`、会话级。
_Avoid_: 登录 cookie、会话 cookie

**ST（Service Ticket）**:
用 TGT 向业务系统换取的一次性票据（`ticket=ST-xxx`），业务系统凭它建立自身会话。
_Avoid_: ticket、登录票

**锁定 TGT**:
多因子策略下，密码校验通过后 TGT 已发放但不可换取 ST 的状态；完成 reauth 后才解锁。
_Avoid_: 无效 TGT、过期 TGT

**reauth（二次认证）**:
多因子策略要求的第二因素认证流程；未完成前 TGT 被锁定，任何换 ST 请求都会被弹回 reauth 页。
_Avoid_: 二次登录、MFA 流程、验证

**reauth 方式（reAuthType）**:
第二因素的认证通道，编号由服务端定义：2=密码、3=短信、4=企微码、8/16=微信扫码、11=邮箱等。
可用方式由服务端按账号渲染（reauth 页 Tab），未渲染 = 未开通 = 切换被拒 + 提交必失败。
_Avoid_: 认证类型、验证方式

**ReauthRequired**:
库的 `login()` 在 TGT 被锁时返回的错误变体，携带 `ReauthContext`（可用方式 + 服务端渲染参数）。
_Avoid_: LoginFailed、登录失败

## reauth 流程要素

**reAuthParams**:
reauth 页服务端渲染的 `var reAuthParams={...}` 对象（44 键），是一切 reauth 参数的唯一来源；
页面可能有多个同名块（数盾 OTP 提示块在前），解析必须取目标键命中最多者。
_Avoid_: 表单字段、隐藏域

**可信设备弹窗（skipTmpReAuth）**:
`isSleepAccount=0` 时提交前出现的确认框：false=仅本次（默认）/ true=信任此设备（服务端持久化
设备指纹）。库将选择权交给调用方（`reauth_trust_device` 配置）。
_Avoid_: 记住我、免认证

**bfp（MULTIFACTOR_BROWSER_FINGERPRINT）**:
多因子风控的设备指纹：浏览器计算 32 位 hex 上报 `GET /bfp/info?bfp=`，服务端只当字符串比对
不校验算法。自动化可自定值；live 实测证明流程不依赖它。
_Avoid_: 设备 ID、浏览器指纹算法

**happyVoyage**:
reauth 流程的会话票据 cookie（base64 密文），随 reauth 状态推进换新值；自动化无需理解其内容。
_Avoid_: reauth token

## 方式类别

**动态码方式**:
3/4/5/11/12/13/15 等"发码后输入验证码"的方式，共用发码接口 `getDynamicCodeByReauth.do`
（`authCodeTypeName` 区分）与提交接口 `reAuthSubmit.do`（验证码明文）。
_Avoid_: 短信方式、验证码方式

**微信扫码方式**:
type 8/16，走外部 OAuth 链路（`combinedLogin.do?type=weixin&reAuth=2` → 微信扫码 → 回调），
**不经过** `reAuthSubmit.do`——扫码动作本身即完成第二因素。扫码动作必须人工（手机微信）。
_Avoid_: 微信登录、扫码登录
