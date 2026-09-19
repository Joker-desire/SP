# 模型说明

闭眼检测用的两个模型都随应用打包在 `models/` 下，**运行时不联网、不调任何 API**。
换模型时注意：下面两个文件的输入尺寸是写死在 `src-tauri/src/blink.rs` 里的常量。

## yunet.onnx（人脸检测，232 KB）

- 出处：OpenCV Zoo — <https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet>
- 许可：Apache-2.0
- 输入：`[1, 3, 640, 640]`，**BGR**、0–255（对齐 OpenCV `FaceDetectorYN` 的口径）
- 输出：`cls_* / obj_* / bbox_* / kps_*`，三个尺度（stride 8 / 16 / 32）

## face_landmark.onnx（人脸关键点，4.9 MB）

- 出处：Google MediaPipe Face Mesh 的 ONNX 转换版 —
  <https://huggingface.co/astaileyyoung/FaceMeshONNX>（该仓库 MIT；原始模型 Apache-2.0）
- 许可：MIT / Apache-2.0
- 输入：`[1, 256, 256, 3]`，**RGB**、0–1，NHWC
- 输出：`Identity` = 478 个关键点的 `(x, y, z)`；`Identity_1` = 「这里真有一张脸」的置信度

## 为什么不用 InsightFace 的 2d106det

它也是 106 点、也好用，但 InsightFace 的预训练模型许可写的是
「仅非商业研究用途」。这个仓库是 MIT 的，打包一个非商用的模型进去
会把使用者也拖进那个限制里，所以没选它。

## 想换更小的模型

可以把 ONNX 量化后再放进来（rten 支持 uint8 激活 + int8 权重）；
改完记得同步 `blink.rs` 里的 `DET_SIZE` / `LMK_SIZE` 和输入归一化方式。
