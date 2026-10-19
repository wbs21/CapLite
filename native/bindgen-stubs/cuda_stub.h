/* CapLite 用：ffmpeg-sys-next 的 hwcontext_wrapper.h 只补了 CUcontext/CUstream，
 * 但这版 FFmpeg（master, avutil 61.13）的 hwcontext_cuda.h 还用到 CUarray 与
 * CUDA_ARRAY3D_DESCRIPTOR（新增 AV_PIX_FMT_CUARRAY 支持）。
 * 通过 BINDGEN_EXTRA_CLANG_ARGS="-include <本文件>" 在解析前注入这两个类型。 */
#ifndef CAP_CUDA_STUB_H
#define CAP_CUDA_STUB_H

typedef struct CUarray_st *CUarray;
typedef struct CUevent_st *CUevent;
typedef int CUdevice;
typedef unsigned long long CUdeviceptr;

typedef struct CUDA_ARRAY3D_DESCRIPTOR_st {
    unsigned int Width;
    unsigned int Height;
    unsigned int Depth;
    unsigned int Format;
    unsigned int NumChannels;
    unsigned int Flags;
} CUDA_ARRAY3D_DESCRIPTOR;

#endif
