# 代码签名计划（2026-10-08）

> **所有者决定（2026-10-08）：不买证书，桌面壳不签名发布；误报提交也先不做（暂缓）。** 现在的方案是尽力降低误报风险，要求和自动检查见 [AV-HYGIENE.md](AV-HYGIENE.md)。下面的证书选项和“每次发布都做”的误报渠道留作以后参考。
> 不签名期间：不写开机启动项（Run 键）；命令行只用普通 `-Command`，不用 `-EncodedCommand`、不改执行策略；本机同意之前不启动任何 shell。
> 资料核实日期均为 2026-10-08，来源链接见各节。

## 为什么要签

没有签名的 exe 会启动 shell、长连外网、读写文件，这和恶意软件的行为特征重合，杀毒软件（Windows Defender、360、火绒、腾讯电脑管家）容易报毒或拦截。签名本身不保证不报毒，但它是信誉的前提：SmartScreen 和国内杀软都按“发布者证书 + 文件”积累信誉。

## 选项

| 方案 | 费用 | 能不能用 | 备注 |
| --- | --- | --- | --- |
| SignPath Foundation（开源免费） | 免费 | 活跃（2026 年仍在接新项目，Microsoft 官方页面推荐）；**对新项目会因“没有用户基础”拒绝**（2026-09 有两例） | 证书主体是“SignPath Foundation”而不是我们；要求 OSI 许可证、没有专有组件、所有成员开 MFA、作者/审核者/批准者角色、每次签名人工批准、在 GitHub 托管的 runner 上构建、项目网站有“代码签名政策”和隐私说明。连接商业云服务本身不被禁止，但隐私条款要求安装时说明并能关闭向用户没选的系统发数据。https://signpath.org/terms 、https://docs.signpath.io/trusted-build-systems/github |
| Azure Artifact Signing（原 Trusted Signing） | Basic 每月约 9.99 美元（5,000 次）；Premium 每月约 99.99 美元 | **中国大陆的个人和公司都不在支持名单**（组织：美、加、欧盟、英、澳、新、日、韩、新加坡等；个人只有美、加） | 不立即带 SmartScreen 信誉；有官方 GitHub Action（只在 Windows runner）。https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart |
| Certum 证书 | 开源版 €49（云）/ €69（卡）；标准 OV €209 / €169；EV €379 / €359 | 可以（个人或公司） | 开源版证书写“Open Source Developer + 姓名”，每月 5,000 次；云签名要手机一次性码，CI 全自动要另想办法。2026-02-27 起证书最长 459 天。https://shop.certum.eu/code-signing.html |
| 普通 OV / EV | OV 约 150–300 美元/年，EV 400 美元以上/年 | 可以 | 2023-06 起私钥必须在硬件令牌或 HSM（可用云 HSM）；2024 起 EV 不再直接绕过 SmartScreen，Microsoft 说不必为此多花钱 |

## 建议的顺序

1. **现在就做（不花钱）**：仓库成员开 MFA；写 `CODE-SIGNING-POLICY`（角色、成员、隐私说明：设备端只连用户自己选的 AgentRouter 网关，不发别的数据）；发布流程只在 GitHub 托管 runner 上构建，产物带版本资源（已加：公司、产品名、版本、版权）。
2. **要尽快发给用户时**：买 Certum 证书（所有者个人名义开源版 €49，或公司名义标准版 €209），私钥放 Certum 云 HSM；CI 构建出未签名 exe，由所有者在本机或受控签名机上签，再上传发布。
3. **有了可见的用户量以后**：申请 SignPath Foundation（https://signpath.org/apply），接入它的 GitHub Action，签名由他们的服务完成。
4. **Azure Artifact Signing**：只有在支持国家有法人主体时才考虑。

## 每次发布都做（暂缓，所有者 2026-10-08 决定先不做）

- 向各家报误报（签不签名都做）：
  - Microsoft：https://www.microsoft.com/en-us/wdsi/filesubmission （选“软件开发者”，附文件、检测名、定义版本）
  - 360：https://open.soft.360.cn/report.php （1–2 个工作日；长期可注册 360 开放平台做软件认证，个人需实名和备案网站）
  - 火绒：论坛「病毒查杀问题反馈 → 样本误报」https://bbs.huorong.cn/forum-44-1.html
  - 腾讯电脑管家：开发者通道未能核实（页面需脚本渲染），先通过 https://guanjia.qq.com/ 的软件平台入口尝试
- 发布说明里写 SHA-256；自动更新只装用钉住的发布公钥验过签的版本（和代码签名证书分开，见 LINKED-DEVICES.md §7）。

## 行为上已经做的（减少被当成恶意软件）

- 不用 `-EncodedCommand`、不用 `-ExecutionPolicy Bypass`；命令以明文 `-Command` 传给 PowerShell。
- 任何命令在本机确认之前都不启动 shell（完全访问也要每个对话先确认一次）。
- 不用挂起进程再恢复的技巧；直接启动后放进作业对象。
- 未签名期间不写注册表开机启动项。
- 只往外连，不开端口；不在临时目录里放或运行 exe；不自我修改。
