name: 问题反馈
description: 哪里不对、期望是什么
labels: ["bug"]
body:
  - type: markdown
    value: |
      感谢反馈。为了让别人能复现，请尽量填全下面几项。
      **不要贴真实照片路径或个人信息**——日志里的路径可以打码成 `~/Photos/...`。
  - type: input
    id: version
    attributes:
      label: 版本
      description: 关于窗口里显示的版本号，或 Release 文件名里的版本
      placeholder: 0.1.0
    validations:
      required: true
  - type: dropdown
    id: os
    attributes:
      label: 系统
      options:
        - macOS（Apple Silicon）
        - macOS（Intel）
        - Windows
        - 其他
    validations:
      required: true
  - type: textarea
    id: what
    attributes:
      label: 发生了什么
      description: 你做了什么、看到了什么、期望看到什么
    validations:
      required: true
  - type: textarea
    id: repro
    attributes:
      label: 复现步骤
      placeholder: |
        1. 选择文件夹 …
        2. 按 P …
        3. …
    validations:
      required: false
  - type: textarea
    id: logs
    attributes:
      label: 日志
      description: 启动失败的话，数据目录下的 startup-error.log 内容很有用（macOS 在 ~/Library/Application Support/SP/）
      render: shell
    validations:
      required: false
