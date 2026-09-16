name: 功能建议
description: 想加什么、为什么需要
labels: ["enhancement"]
body:
  - type: textarea
    id: need
    attributes:
      label: 你想解决什么问题
      description: 说场景而不是说方案——「连拍里选一张太慢」比「加个 AI 评分」更容易讨论
    validations:
      required: true
  - type: textarea
    id: now
    attributes:
      label: 现在是怎么绕过去的
    validations:
      required: false
  - type: textarea
    id: idea
    attributes:
      label: 如果有具体想法
      description: 界面长什么样、快捷键怎么安排，都可以写
    validations:
      required: false
