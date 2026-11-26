#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreImage/CoreImage.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

typedef void (*jrp_au_cb)(const uint8_t *bytes, int32_t len, uint8_t idr, uint64_t ts, void *ctx);
typedef void (*jrp_audio_cb)(const uint8_t *bytes, int32_t len, uint64_t ts, void *ctx);
typedef void (*jrp_input_cb)(uint8_t kind, int32_t a, int32_t b, int32_t c, void *ctx);
typedef void (*jrp_menu_cb)(uint8_t kind, int32_t value, void *ctx);

/// 带提示地询问辅助功能授权。只调 AXIsProcessTrusted 不会弹系统窗口，
/// 用户会以为按钮没反应；这里必须用 WithOptions 并带上 Prompt。
void jrp_request_accessibility(void) {
    NSDictionary *options = @{(__bridge NSString *)kAXTrustedCheckOptionPrompt: @YES};
    AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)options);
}

static jrp_menu_cb g_menu;
static void *g_menu_ctx;
static char *g_display_names;

static jrp_au_cb g_au;
static void *g_au_ctx;
static jrp_audio_cb g_audio;
static void *g_audio_ctx;
static atomic_int g_audio_on = 0;
/// 音频这一层的代次。视频的 g_epoch 只在采集流整体 stop/start 时推进，
/// 同一代采集里先 stop 音频再 start 会保持 epoch 不变：等锁的旧 PCM 醒来
/// 时 g_audio_on 又变回 1，就会被编进新一段声音。音频 stop/reset 各自
/// 推进这个计数，编码路径在锁内比对，旧样本一律丢掉。
static atomic_int g_audio_gen = 0;
static atomic_int g_audio_supported = 0;
// SCK 是否已经真正尝试挂过音频输出。挂之前只能按系统权限乐观判断，
// 挂失败之后下一次会话的能力位要如实收掉。
static atomic_int g_audio_probed = 0;
static jrp_input_cb g_input;
static void *g_input_ctx;
static atomic_int g_running = 0;
/// 哪个 epoch 把 g_running 置成了 1。旧流的启动回调只能清掉自己留下的
/// running，不能把下一段采集的状态一起清掉。
static atomic_int g_running_epoch = 0;
static atomic_int g_starting = 0;
static atomic_int g_failed = 0;
static atomic_int g_epoch = 0;
static atomic_int g_hardware = -1;
static atomic_int g_hangup = 0;
static atomic_int g_focused = 0;
static atomic_int g_open_req = 0;
static atomic_int g_indicator_req = 0;
static atomic_int g_cover_req = 0;
static atomic_int g_quality_req = 0;
static atomic_int g_display_sel_req = -1;
static atomic_int g_displays_req = 0;
static pthread_mutex_t g_display_mu = PTHREAD_MUTEX_INITIALIZER;
static NSPopUpButton *g_quality_pop;
static NSPopUpButton *g_display_pop;
static uint32_t g_req_w = 1280;
static uint32_t g_req_h = 720;
typedef struct JrpPendingFrame {
    uint8_t *bytes;
    int32_t len;
    uint8_t idr;
    struct JrpPendingFrame *next;
} JrpPendingFrame;
static JrpPendingFrame *g_pending_head;
static JrpPendingFrame *g_pending_tail;
static int g_pending_count;
static bool g_wait_for_idr;
static atomic_int g_need_keyframe = 0;
static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;

static VTCompressionSessionRef g_session;
static int g_enc_w;
static int g_enc_h;
static int g_bitrate = 4000000;
static atomic_int g_fps_req = 60;
static double g_last_emit_s = 0.0;
static uint8_t *g_sps;
static size_t g_sps_len;
static uint8_t *g_pps;
static size_t g_pps_len;
static uint64_t g_last_param_us;
static pthread_mutex_t g_audio_enc_mu = PTHREAD_MUTEX_INITIALIZER;
static AVAudioConverter *g_audio_enc;
static AVAudioFormat *g_audio_enc_in;
static AVAudioFormat *g_audio_enc_out;

/// 释放采集编码器。调用方必须持有 g_audio_enc_mu。
/// AVAudioConverter 在输入帧数不是 1024（一个 AAC 包）的整数倍时，会把
/// 不足一包的尾部 PCM 留在内部。只清回调和 g_audio_on 而不释放 converter
/// 的话，下一段会话 ASBD 相同时格式比较会判定未变，直接复用旧实例，
/// 上一段会话残留的 PCM 就会被编进新会话的第一个 AAC 包。
/// 这里只做 ARC 下的 nil 赋值（即释放），不调用任何可能回到编码路径的 API。
static void audio_enc_release_locked(void) {
    g_audio_enc = nil;
    g_audio_enc_in = nil;
    g_audio_enc_out = nil;
}

@interface JrpCap : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, strong) SCStream *stream;
/// 这条流属于哪一代采集。stop 之后旧流还可能吐几帧（声音也会），
/// 回调里先按 epoch 把旧流整条丢掉，别混进新会话。
@property(nonatomic, assign) int epoch;
@end

static JrpCap *g_cap;

static atomic_int g_force_idr = 0;

/// AVAudioConverter 的输入块一次只取走它请求的帧数；把整块更大的 PCM
/// 直接交回去时，多出来的尾巴会被编码器丢掉。这里按请求切一块副本，
/// 剩下的帧下一轮再喂，避免一块 SCK 缓冲跨多个 AAC 帧时丢样本。
static AVAudioPCMBuffer *audio_chunk(AVAudioPCMBuffer *src, AVAudioFrameCount offset, AVAudioFrameCount frames) {
    if (!src || frames == 0) {
        return nil;
    }
    AVAudioPCMBuffer *dst = [[AVAudioPCMBuffer alloc] initWithPCMFormat:src.format frameCapacity:frames];
    if (!dst) {
        return nil;
    }
    dst.frameLength = frames;
    UInt32 nbuf = MIN(src.audioBufferList->mNumberBuffers, dst.audioBufferList->mNumberBuffers);
    for (UInt32 i = 0; i < nbuf; i++) {
        const uint8_t *from = src.audioBufferList->mBuffers[i].mData;
        uint8_t *to = dst.audioBufferList->mBuffers[i].mData;
        if (!from || !to) {
            continue;
        }
        // 交错格式一块就是整帧，非交错格式一块是一个声道；按目标缓冲的
        // 每帧字节数算，两种布局都不会越界。
        size_t per_frame = (size_t)dst.audioBufferList->mBuffers[i].mDataByteSize / (size_t)frames;
        memcpy(to, from + (size_t)offset * per_frame, (size_t)frames * per_frame);
    }
    return dst;
}

/// ScreenCaptureKit 给的是交错或非交错的 LPCM。先按它自己的格式装进
/// AVAudioPCMBuffer，再交给系统 AAC 编码器；不自己写重采样和声道变换。
/// epoch 是调用方入口处核对过的采集代际。入口检查和真正拿到
/// g_audio_enc_mu 之间隔着一次等锁，期间整代采集可能已经被 stop 换掉，
/// 所以锁内还要用这个值再核一次。
static void encode_audio_sample(CMSampleBufferRef sb, int epoch, int audio_gen) {
    if (!CMSampleBufferDataIsReady(sb)) {
        return;
    }
    CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sb);
    const AudioStreamBasicDescription *asbd = fmt ? CMAudioFormatDescriptionGetStreamBasicDescription(fmt) : NULL;
    if (!asbd || asbd->mChannelsPerFrame == 0) {
        return;
    }
    CMItemCount frames = CMSampleBufferGetNumSamples(sb);
    if (frames <= 0) {
        return;
    }

    pthread_mutex_lock(&g_audio_enc_mu);
    // stop/reset 会在同一把锁下清零 g_audio_on 并释放 converter。等锁期间
    // 它们可能已经清完了，这一步之后才到的采样（旧 epoch 的尾巴）必须直接
    // 丢掉；否则这里会重新建出 converter 并再次把 PCM 留在内部，下一段
    // 会话照样会复用旧状态。
    // 只比 g_audio_on 不够：旧样本卡在锁上时，stop 之后下一次 start 会把
    // g_audio_on 重新置 1，旧 PCM 就会进新会话。入口核对过的 epoch 和当前
    // g_epoch 不一致说明这一帧属于上一代采集；g_audio_gen 再挡住同一代采集里
    // 音频 stop → start 的情况。两者都放进同一临界区比，不再多拿一把锁。
    if (epoch != atomic_load(&g_epoch) || audio_gen != atomic_load(&g_audio_gen)
        || !atomic_load(&g_audio_on) || !g_audio) {
        pthread_mutex_unlock(&g_audio_enc_mu);
        return;
    }
    // 默认输出设备切换时 SCK 可能改采样率或声道数。旧 converter 继续用
    // 会把新布局按旧长度拷进缓冲，轻则变调，重则把未初始化数据当噪声发出去。
    BOOL fmt_changed = !g_audio_enc || !g_audio_enc_in
        || !g_audio_enc_in.streamDescription
        || memcmp(g_audio_enc_in.streamDescription, asbd, sizeof(*asbd)) != 0;
    if (fmt_changed) {
        AVAudioFormat *in = [[AVAudioFormat alloc] initWithStreamDescription:asbd];
        NSDictionary *settings = @{
            AVFormatIDKey: @(kAudioFormatMPEG4AAC),
            AVSampleRateKey: @48000,
            // 协议固定 AAC-LC / 48 kHz / 双声道，播放端的 AudioSpecificConfig
            // 也写死成双声道。输入设备若变成单声道，必须让转换器补成双声道，
            // 否则远端解码器会一帧都解不出来。
            AVNumberOfChannelsKey: @2,
            AVEncoderBitRateKey: @128000,
        };
        AVAudioFormat *out = [[AVAudioFormat alloc] initWithSettings:settings];
        AVAudioConverter *conv = (in && out) ? [[AVAudioConverter alloc] initFromFormat:in toFormat:out] : nil;
        if (!conv) {
            g_audio_enc = nil;
            g_audio_enc_in = nil;
            g_audio_enc_out = nil;
            pthread_mutex_unlock(&g_audio_enc_mu);
            return;
        }
        g_audio_enc_in = in;
        g_audio_enc_out = out;
        g_audio_enc = conv;
    }

    AVAudioPCMBuffer *pcm = [[AVAudioPCMBuffer alloc] initWithPCMFormat:g_audio_enc_in frameCapacity:(AVAudioFrameCount)frames];
    if (!pcm) {
        pthread_mutex_unlock(&g_audio_enc_mu);
        return;
    }
    pcm.frameLength = (AVAudioFrameCount)frames;

    size_t needed = 0;
    CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb, &needed, NULL, 0, NULL, NULL, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, NULL);
    if (needed < sizeof(AudioBufferList)) {
        needed = sizeof(AudioBufferList);
    }
    AudioBufferList *src = malloc(needed);
    CMBlockBufferRef block = NULL;
    OSStatus got = noErr;
    if (src) {
        got = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb, NULL, src, needed, NULL, NULL, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &block);
    }
    if (!src || got != noErr) {
        free(src);
        pthread_mutex_unlock(&g_audio_enc_mu);
        return;
    }
    UInt32 nbuf = src->mNumberBuffers;
    if (nbuf > pcm.audioBufferList->mNumberBuffers) {
        nbuf = pcm.audioBufferList->mNumberBuffers;
    }
    for (UInt32 i = 0; i < nbuf; i++) {
        size_t n = src->mBuffers[i].mDataByteSize;
        if (n > pcm.audioBufferList->mBuffers[i].mDataByteSize) {
            n = pcm.audioBufferList->mBuffers[i].mDataByteSize;
        }
        if (src->mBuffers[i].mData && pcm.audioBufferList->mBuffers[i].mData) {
            memcpy(pcm.audioBufferList->mBuffers[i].mData, src->mBuffers[i].mData, n);
        }
    }
    if (block) {
        CFRelease(block);
    }
    free(src);

    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    uint64_t ts = CMTIME_IS_NUMERIC(pts)
        ? (uint64_t)(CMTimeGetSeconds(pts) * 1000000.0)
        : (uint64_t)(CACurrentMediaTime() * 1000000.0);
    // 一块 SCK 缓冲可能跨多个 AAC 帧，采样率不是 48 kHz 时需要的输入帧数
    // 也不是 1024 的整数倍。一次 convert 只会取走输入块请求的帧数，所以
    // 按请求切片并循环，直到这块 PCM 全部交给编码器。
    AVAudioFrameCount submitted = 0;
    uint64_t emitted_frames = 0;
    while (submitted < pcm.frameLength) {
        AVAudioCompressedBuffer *out = [[AVAudioCompressedBuffer alloc] initWithFormat:g_audio_enc_out packetCapacity:8 maximumPacketSize:4096];
        if (!out) {
            break;
        }
        __block BOOL fed = NO;
        __block AVAudioFrameCount pos = submitted;
        NSError *err = nil;
        AVAudioConverterOutputStatus status = [g_audio_enc
            convertToBuffer:out
                     error:&err
            withInputFromBlock:^AVAudioBuffer *(AVAudioPacketCount inNumberOfPackets, AVAudioConverterInputStatus *outStatus) {
                if (fed) {
                    *outStatus = AVAudioConverterInputStatus_NoDataNow;
                    return nil;
                }
                AVAudioFrameCount avail = pcm.frameLength - pos;
                if (avail == 0) {
                    *outStatus = AVAudioConverterInputStatus_NoDataNow;
                    return nil;
                }
                AVAudioFrameCount want = (AVAudioFrameCount)inNumberOfPackets;
                if (want == 0 || want > avail) {
                    want = avail;
                }
                AVAudioPCMBuffer *chunk = nil;
                if (pos == 0 && want == pcm.frameLength) {
                    chunk = pcm;
                } else {
                    chunk = audio_chunk(pcm, pos, want);
                }
                if (!chunk) {
                    *outStatus = AVAudioConverterInputStatus_NoDataNow;
                    return nil;
                }
                pos += want;
                fed = YES;
                *outStatus = AVAudioConverterInputStatus_HaveData;
                return chunk;
            }];
        AVAudioFrameCount before = submitted;
        submitted = pos;
        if (status == AVAudioConverterOutputStatus_Error) {
            break;
        }
        // 回调指针和 ctx 同样受 g_audio_enc_mu 保护，并且每次发之前重看一次
        // g_audio_on：stop 拿到锁返回后，这一帧剩下的包就不能再进上层队列，
        // 否则旧会话的声音会漏到下一段会话里。
        jrp_audio_cb cb = atomic_load(&g_audio_on) ? g_audio : NULL;
        void *ctx = g_audio_ctx;
        const uint8_t *base = (const uint8_t *)out.data;
        AudioStreamPacketDescription *pd = out.packetDescriptions;
        for (AVAudioPacketCount p = 0; p < out.packetCount && cb; p++) {
            const uint8_t *at = base;
            int32_t len = 0;
            if (pd) {
                at = base + pd[p].mStartOffset;
                len = (int32_t)pd[p].mDataByteSize;
            } else if (p == 0) {
                len = (int32_t)out.byteLength;
            }
            if (len > 0) {
                // 每个 AAC 包固定 1024 个采样，21.3ms；时间戳只用于排抖动缓冲。
                cb(at, len, ts + emitted_frames * 1000000 / 48000, ctx);
            }
            emitted_frames += 1024;
        }
        if (submitted == before && out.packetCount == 0) {
            // 编码器既没拿走输入也没产出，继续循环只会空转。
            break;
        }
    }
    pthread_mutex_unlock(&g_audio_enc_mu);
}

static void emit_au(const uint8_t *bytes, int32_t len, uint8_t idr, uint64_t ts) {
    if (!atomic_load(&g_running)) {
        return;
    }
    jrp_au_cb cb = g_au;
    if (cb && bytes && len > 0) {
        cb(bytes, len, idr, ts, g_au_ctx);
    }
}

static int annex_from_sample(CMSampleBufferRef sb, uint8_t **out_bytes, int32_t *out_len, uint8_t *idr) {
    CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sb);
    if (!fmt) {
        return -1;
    }
    const uint8_t *ps = NULL;
    size_t ps_len = 0;
    if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 0, &ps, &ps_len, NULL, NULL) == noErr && ps_len > 0) {
        free(g_sps);
        g_sps = malloc(ps_len);
        memcpy(g_sps, ps, ps_len);
        g_sps_len = ps_len;
    }
    if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 1, &ps, &ps_len, NULL, NULL) == noErr && ps_len > 0) {
        free(g_pps);
        g_pps = malloc(ps_len);
        memcpy(g_pps, ps, ps_len);
        g_pps_len = ps_len;
    }
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    Boolean sync = true;
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef dict = CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync);
        if (not_sync && CFBooleanGetValue(not_sync)) {
            sync = false;
        }
    }
    *idr = sync ? 1 : 0;
    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sb);
    if (!block) {
        return -1;
    }
    size_t total = 0;
    char *data = NULL;
    if (CMBlockBufferGetDataPointer(block, 0, NULL, &total, &data) != kCMBlockBufferNoErr || !data) {
        return -1;
    }
    size_t cap = total + g_sps_len + g_pps_len + 64;
    uint8_t *out = malloc(cap);
    size_t used = 0;
    uint64_t now = (uint64_t)(CACurrentMediaTime() * 1000000.0);
    int with_param = *idr || (g_last_param_us == 0) || (now - g_last_param_us > 1000000);
    if (with_param && g_sps && g_pps) {
        uint8_t sc[4] = {0, 0, 0, 1};
        memcpy(out + used, sc, 4);
        used += 4;
        memcpy(out + used, g_sps, g_sps_len);
        used += g_sps_len;
        memcpy(out + used, sc, 4);
        used += 4;
        memcpy(out + used, g_pps, g_pps_len);
        used += g_pps_len;
        g_last_param_us = now;
    }
    size_t off = 0;
    while (off + 4 <= total) {
        uint32_t nlen = ((uint8_t)data[off] << 24) | ((uint8_t)data[off + 1] << 16) | ((uint8_t)data[off + 2] << 8) | (uint8_t)data[off + 3];
        off += 4;
        if (nlen == 0 || off + nlen > total) {
            break;
        }
        if (used + 4 + nlen > cap) {
            cap = used + 4 + nlen + 64;
            out = realloc(out, cap);
        }
        out[used++] = 0;
        out[used++] = 0;
        out[used++] = 0;
        out[used++] = 1;
        memcpy(out + used, data + off, nlen);
        used += nlen;
        off += nlen;
    }
    if (used < 5) {
        free(out);
        return -1;
    }
    *out_bytes = out;
    *out_len = (int32_t)used;
    return 0;
}

static void encode_cb(void *refcon, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    (void)refcon;
    (void)source;
    (void)flags;
    if (status != noErr || !sb || !CMSampleBufferDataIsReady(sb)) {
        return;
    }
    uint8_t *bytes = NULL;
    int32_t len = 0;
    uint8_t idr = 0;
    if (annex_from_sample(sb, &bytes, &len, &idr) != 0) {
        return;
    }
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    uint64_t ts = 0;
    if (CMTIME_IS_NUMERIC(pts)) {
        ts = (uint64_t)(CMTimeGetSeconds(pts) * 1000000.0);
    }
    emit_au(bytes, len, idr, ts);
    free(bytes);
}

static int ensure_encoder(int width, int height) {
    width &= ~1;
    height &= ~1;
    if (width < 2 || height < 2) {
        return -1;
    }
    if (g_session && g_enc_w == width && g_enc_h == height) {
        return 0;
    }
    if (g_session) {
        VTCompressionSessionInvalidate(g_session);
        CFRelease(g_session);
        g_session = NULL;
    }
    NSDictionary *spec = @{
        (id)kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder : @YES
    };
    OSStatus st = VTCompressionSessionCreate(NULL, width, height, kCMVideoCodecType_H264, (__bridge CFDictionaryRef)spec, NULL, NULL, encode_cb, NULL, &g_session);
    if (st != noErr) {
        st = VTCompressionSessionCreate(NULL, width, height, kCMVideoCodecType_H264, NULL, NULL, NULL, encode_cb, NULL, &g_session);
        atomic_store(&g_hardware, 0);
    }
    if (st != noErr || !g_session) {
        return -1;
    }
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel);
    int32_t br = g_bitrate;
    CFNumberRef rate = CFNumberCreate(NULL, kCFNumberSInt32Type, &br);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_AverageBitRate, rate);
    CFRelease(rate);
    int32_t window = br / 64;
    if (window < 1) {
        window = 1;
    }
    double period = 0.125;
    CFNumberRef bytes = CFNumberCreate(NULL, kCFNumberSInt32Type, &window);
    CFNumberRef seconds = CFNumberCreate(NULL, kCFNumberDoubleType, &period);
    CFTypeRef limits_items[] = {bytes, seconds};
    CFArrayRef limits = CFArrayCreate(NULL, limits_items, 2, &kCFTypeArrayCallBacks);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_DataRateLimits, limits);
    CFRelease(limits);
    CFRelease(bytes);
    CFRelease(seconds);
    int32_t interval = 120;
    CFNumberRef gop = CFNumberCreate(NULL, kCFNumberSInt32Type, &interval);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_MaxKeyFrameInterval, gop);
    CFRelease(gop);
    int fps = atomic_load(&g_fps_req);
    if (fps < 1) {
        fps = 60;
    }
    CFNumberRef rate_fps = CFNumberCreate(NULL, kCFNumberSInt32Type, &fps);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_ExpectedFrameRate, rate_fps);
    CFRelease(rate_fps);
    double idr_sec = 2.0;
    CFNumberRef idr = CFNumberCreate(NULL, kCFNumberDoubleType, &idr_sec);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, idr);
    CFRelease(idr);
    VTCompressionSessionPrepareToEncodeFrames(g_session);
    CFBooleanRef hw = NULL;
    if (VTSessionCopyProperty(g_session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hw) == noErr && hw) {
        atomic_store(&g_hardware, CFBooleanGetValue(hw) ? 1 : 0);
        CFRelease(hw);
    }
    g_enc_w = width;
    g_enc_h = height;
    return 0;
}

@implementation JrpCap
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer ofType:(SCStreamOutputType)type {
    (void)stream;
    // stop 换流之后，旧 SCStream 的样本可能还会到。带着旧时间戳的声音和
    // 画面混进新会话比丢几帧更糟，这里直接按代际丢掉。
    if (self.epoch != atomic_load(&g_epoch)) {
        return;
    }
    if (type == SCStreamOutputTypeAudio) {
        if (atomic_load(&g_audio_on)) {
            // 代次在入口就取。等锁期间可能已经被 stop/reset 换掉，
            // 锁内再比对一次才能把旧样本挡在新会话外面。
            encode_audio_sample(sampleBuffer, self.epoch, atomic_load(&g_audio_gen));
        }
        return;
    }
    if (type != SCStreamOutputTypeScreen || !CMSampleBufferDataIsReady(sampleBuffer)) {
        return;
    }
    CVImageBufferRef image = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!image) {
        return;
    }
    size_t w = CVPixelBufferGetWidth(image);
    size_t h = CVPixelBufferGetHeight(image);
    if (ensure_encoder((int)w, (int)h) != 0) {
        return;
    }
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
    if (!CMTIME_IS_NUMERIC(pts)) {
        pts = CMTimeMake((int64_t)(CACurrentMediaTime() * 1000000.0), 1000000);
    }
    int fps = atomic_load(&g_fps_req);
    if (fps > 0 && fps < 120) {
        double now_s = CMTimeGetSeconds(pts);
        double interval = 1.0 / (double)fps;
        // 降帧靠丢样本，不改采集配置，避免重配期间画面断档。
        if (now_s > 0 && g_last_emit_s > 0 && (now_s - g_last_emit_s) < interval * 0.9) {
            return;
        }
        g_last_emit_s = now_s;
    }
    CFDictionaryRef props = NULL;
    if (atomic_exchange(&g_force_idr, 0)) {
        CFTypeRef keys[] = {kVTEncodeFrameOptionKey_ForceKeyFrame};
        CFTypeRef vals[] = {kCFBooleanTrue};
        props = CFDictionaryCreate(NULL, keys, vals, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    }
    VTEncodeInfoFlags info = 0;
    VTCompressionSessionEncodeFrame(g_session, image, pts, kCMTimeInvalid, props, NULL, &info);
    if (props) {
        CFRelease(props);
    }
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    (void)error;
    // 旧流的错误不能算到新会话头上。
    if (self.epoch != atomic_load(&g_epoch)) {
        return;
    }
    // 主动 stop 时会先把 running 清零；只有“本来在跑、自己停了”才算失败，
    // 否则正常关流会被误判成需要重启。
    if (atomic_load(&g_running)) {
        atomic_store(&g_failed, 1);
        atomic_store(&g_running, 0);
    }
}
@end

int jrp_screen_granted(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

int jrp_display_count(void) {
    uint32_t count = 0;
    if (CGGetActiveDisplayList(0, NULL, &count) != kCGErrorSuccess) {
        return 0;
    }
    return (int)count;
}

void jrp_display_at(int index, int32_t *ox, int32_t *oy, uint32_t *w, uint32_t *h, uint32_t *scale_milli) {
    uint32_t count = 0;
    CGDirectDisplayID ids[8];
    CGGetActiveDisplayList(8, ids, &count);
    if (index < 0 || (uint32_t)index >= count) {
        *ox = 0;
        *oy = 0;
        *w = 0;
        *h = 0;
        *scale_milli = 1000;
        return;
    }
    CGRect b = CGDisplayBounds(ids[index]);
    uint32_t pw = (uint32_t)CGDisplayPixelsWide(ids[index]);
    uint32_t ph = (uint32_t)CGDisplayPixelsHigh(ids[index]);
    uint32_t scale = 1000;
    if (b.size.width > 1.0) {
        scale = (uint32_t)((pw * 1000.0) / b.size.width);
    }
    if (scale < 1000) {
        scale = 1000;
    }
    *scale_milli = scale;
    *ox = (int32_t)(b.origin.x * (scale / 1000.0));
    *oy = (int32_t)(b.origin.y * (scale / 1000.0));
    *w = pw;
    *h = ph;
}

static atomic_int g_display_pick = 0;

int jrp_audio_ready(void) {
    // SCStream 的系统音频采集从 macOS 13 才有；12.x 上不能报这个能力。
    if (@available(macOS 13.0, *)) {
    } else {
        return 0;
    }
    // 还没真正挂过音频输出时只能看录屏权限；挂过一次失败后要如实返回 0。
    if (atomic_load(&g_audio_probed)) {
        return atomic_load(&g_audio_supported) ? 1 : 0;
    }
    return jrp_screen_granted() ? 1 : 0;
}

int jrp_audio_start(jrp_audio_cb cb, void *ctx) {
    if (!jrp_screen_granted()) {
        return 1;
    }
    // 已经探测过而且音频输出挂不上时，不要假装启动成功；节点会按失败
    // 走有限重试，而不是一直打印“开始采集系统声音”。
    if (atomic_load(&g_audio_probed) && !atomic_load(&g_audio_supported)) {
        return 2;
    }
    // 还没探测过说明采集流刚启动。先记下回调并认为已启动，音频输出挂上后
    // 采样自然进来；挂失败会把 g_audio_supported 置 0，节点随后会重启。
    // 回调指针和采集中间态用同一把锁，避免采集线程读到半写入的 ctx。
    pthread_mutex_lock(&g_audio_enc_mu);
    g_audio = cb;
    g_audio_ctx = ctx;
    atomic_store(&g_audio_on, 1);
    pthread_mutex_unlock(&g_audio_enc_mu);
    return 0;
}

void jrp_audio_stop(void) {
    // 必须和采集编码路径共用一把锁：拿到锁再清，返回之后就不会再有旧回调
    // 或旧 ctx 被使用，上层清队列也不会被晚到的帧重新塞满。
    // encoder 也在同一把锁下释放。编码回调整段（含 convertToBuffer）都持锁，
    // 所以这里会等它退出后才清，不会出现回调还在用 converter 就被释放的
    // use-after-free；锁内只做 nil 赋值，不调用会回到编码路径的 API。
    pthread_mutex_lock(&g_audio_enc_mu);
    // 推进音频代次：同一代采集里 stop 之后可能马上又 start，只清 g_audio_on
    // 挡不住锁外还在排队的旧样本。
    atomic_fetch_add(&g_audio_gen, 1);
    atomic_store(&g_audio_on, 0);
    g_audio = NULL;
    g_audio_ctx = NULL;
    audio_enc_release_locked();
    pthread_mutex_unlock(&g_audio_enc_mu);
}

/// 采集端是否真的在收音频。切屏、采集失败重建期间节点用它发现
/// `audio_started` 已经过期，避免整个会话静音。
int jrp_audio_running(void) {
    // 采集流刚启动、音频输出还没挂完时不能判死，否则节点会在下一次
    // 50ms 轮询里把刚起来的音频停掉。真正挂失败后 g_audio_supported=0。
    return atomic_load(&g_audio_on)
        && (!atomic_load(&g_audio_probed) || atomic_load(&g_audio_supported));
}

int jrp_capture_start(int32_t bitrate, int32_t display_index, jrp_au_cb cb, void *ctx) {
    if (!jrp_screen_granted()) {
        return 1;
    }
    if (atomic_load(&g_running) || atomic_load(&g_starting)) {
        return 0;
    }
    g_au = cb;
    g_au_ctx = ctx;
    if (bitrate > 0) {
        g_bitrate = bitrate;
    }
    atomic_store(&g_display_pick, display_index);
    atomic_store(&g_starting, 1);
    int epoch = atomic_load(&g_epoch);
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                             onScreenWindowsOnly:YES
                                               completionHandler:^(SCShareableContent *content, NSError *error) {
        if (atomic_load(&g_epoch) != epoch) {
            // 这次启动已经被 stop 或下一次启动取代。不能再碰 g_starting /
            // g_failed，否则旧回调会把新一次启动的“正在启动”清掉，节点下一拍
            // 又发一次启动，最后两条 SCStream 同时跑（声音也会重复）。
            return;
        }
        if (error || content.displays.count == 0) {
            atomic_store(&g_starting, 0);
            atomic_store(&g_failed, 1);
            return;
        }
        int pick = atomic_load(&g_display_pick);
        if (pick < 0 || (NSUInteger)pick >= content.displays.count) {
            pick = 0;
        }
        SCDisplay *display = content.displays[(NSUInteger)pick];
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = (size_t)display.width;
        config.height = (size_t)display.height;
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        config.showsCursor = YES;
        config.queueDepth = 3;
        config.minimumFrameInterval = CMTimeMake(1, 60);
        // 系统回环，不是麦克风。SCStream 的音频属性从 macOS 13 才有；
        // 12.x 上调用这些 setter 会直接 unrecognized selector 崩溃。
        BOOL audio_available = NO;
        if (@available(macOS 13.0, *)) {
            config.capturesAudio = YES;
            config.sampleRate = 48000;
            config.channelCount = 2;
            config.excludesCurrentProcessAudio = YES;
            audio_available = YES;
        } else {
            atomic_store(&g_audio_supported, 0);
            atomic_store(&g_audio_probed, 1);
        }
        JrpCap *cap = [JrpCap new];
        cap.epoch = epoch;
        SCStream *stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:cap];
        NSError *add_err = nil;
        dispatch_queue_t q = dispatch_queue_create("jrp.capture", DISPATCH_QUEUE_SERIAL);
        [stream addStreamOutput:cap type:SCStreamOutputTypeScreen sampleHandlerQueue:q error:&add_err];
        if (add_err) {
            if (atomic_load(&g_epoch) == epoch) {
                atomic_store(&g_starting, 0);
                atomic_store(&g_failed, 1);
            }
            return;
        }
        if (audio_available) {
            NSError *audio_err = nil;
            [stream addStreamOutput:cap type:SCStreamOutputTypeAudio sampleHandlerQueue:q error:&audio_err];
            atomic_store(&g_audio_supported, audio_err == nil ? 1 : 0);
            atomic_store(&g_audio_probed, 1);
            if (audio_err) {
                // 声音失败不影响画面；能力协商会在下一次会话里把 audio 位收掉。
                NSLog(@"Jaryn Remote: audio stream unavailable: %@", audio_err);
            }
        }
        // 建流的这几步不是原子的：期间可能已经 stop，或者又起了一次采集。
        // 旧回调不能再把流挂到 g_cap / g_running 上，否则两条 SCStream 会同时
        // 采集（声音也会重复），而 stop 只关得掉其中一条。
        if (atomic_load(&g_epoch) != epoch) {
            return;
        }
        cap.stream = stream;
        g_cap = cap;
        atomic_store(&g_running_epoch, epoch);
        atomic_store(&g_running, 1);
        [stream startCaptureWithCompletionHandler:^(NSError *start_err) {
            if (atomic_load(&g_epoch) != epoch) {
                // 这条流属于已经被 stop 的旧会话。只清掉自己置上的 running，
                // 不要碰新会话的 g_running / g_starting，再关掉自己。
                if (atomic_load(&g_running_epoch) == epoch) {
                    atomic_store(&g_running, 0);
                }
                [stream stopCaptureWithCompletionHandler:^(NSError *stop_err) {
                    (void)stop_err;
                }];
                return;
            }
            atomic_store(&g_starting, 0);
            if (start_err) {
                atomic_store(&g_running, 0);
                atomic_store(&g_failed, 1);
                [stream stopCaptureWithCompletionHandler:^(NSError *stop_err) {
                    (void)stop_err;
                }];
            }
        }];
    }];
    return 0;
}

void jrp_capture_stop(void) {
    atomic_fetch_add(&g_epoch, 1);
    atomic_store(&g_running, 0);
    atomic_store(&g_starting, 0);
    atomic_store(&g_failed, 0);
    // 关流时音频回调也要一起收掉。只清 g_audio_on 的话，正在编码的那一帧
    // 还会把本次会话的声音塞回上层队列，下一段会话开头会先放出旧声音。
    // 同锁释放 converter：它内部缓存的尾部 PCM 属于这条流，不能带进下一条。
    pthread_mutex_lock(&g_audio_enc_mu);
    // 推进音频代次：同一代采集里 stop 之后可能马上又 start，只清 g_audio_on
    // 挡不住锁外还在排队的旧样本。
    atomic_fetch_add(&g_audio_gen, 1);
    atomic_store(&g_audio_on, 0);
    g_audio = NULL;
    g_audio_ctx = NULL;
    audio_enc_release_locked();
    pthread_mutex_unlock(&g_audio_enc_mu);
    SCStream *stream = g_cap.stream;
    g_cap = nil;
    if (stream) {
        [stream stopCaptureWithCompletionHandler:^(NSError *error) {
            (void)error;
        }];
    }
    if (g_session) {
        VTCompressionSessionInvalidate(g_session);
        CFRelease(g_session);
        g_session = NULL;
    }
    g_au = NULL;
}

void jrp_capture_force_idr(void) {
    atomic_store(&g_force_idr, 1);
}

void jrp_capture_set_bitrate(int32_t bitrate) {
    if (bitrate < 500000) {
        bitrate = 500000;
    }
    g_bitrate = bitrate;
    if (!g_session) {
        return;
    }
    CFNumberRef rate = CFNumberCreate(NULL, kCFNumberSInt32Type, &bitrate);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_AverageBitRate, rate);
    CFRelease(rate);
}

void jrp_capture_set_fps(int32_t fps) {
    if (fps < 1) {
        fps = 1;
    }
    if (fps > 120) {
        fps = 120;
    }
    atomic_store(&g_fps_req, fps);
    if (!g_session) {
        return;
    }
    CFNumberRef rate = CFNumberCreate(NULL, kCFNumberSInt32Type, &fps);
    VTSessionSetProperty(g_session, kVTCompressionPropertyKey_ExpectedFrameRate, rate);
    CFRelease(rate);
}

int jrp_capture_running(void) {
    return atomic_load(&g_running);
}

int jrp_capture_failed(void) {
    return atomic_load(&g_failed);
}

int jrp_capture_hardware(void) {
    return atomic_load(&g_hardware);
}

// ---------------------------------------------------------------------------
// 远端声音播放
//
// 解码用系统 AAC 解码器，输出交给 AVAudioEngine。播放放在单独串行队列，
// 不在采集回调或 UI 线程里做解码。缓冲压到 12 帧以内，超过就丢声音追画面。
// ---------------------------------------------------------------------------
static dispatch_queue_t g_play_q;
static AVAudioEngine *g_play_engine;
static AVAudioPlayerNode *g_play_node;
static AVAudioFormat *g_play_aac;
static AVAudioFormat *g_play_pcm;
static AVAudioConverter *g_play_dec;
static id g_play_obs;
static atomic_int g_play_scheduled = 0;
/// 每次丢旧音频追赶时递增。旧的 completionHandler 只减自己那一代，
/// 否则清空计数后旧回调会把计数减成负数。
static atomic_int g_play_gen = 0;
static atomic_int g_play_failed = 0;
static atomic_int g_play_warned = 0;
/// 1024 个 48k 采样 = 21333 微秒。播放端用它判断缺帧和迟到帧。
static const uint64_t kAudioFrameUs = 21333;
/// 播放缓冲上限 80 ms，约 4 帧。超过就丢旧声音追当前时间。
static const int kAudioMaxScheduled = 4;
/// 落后超过 8 帧（约 170 ms）就不可能是网络乱序的迟到包，只能是
/// 对端重开了采集、时间戳从 0 重新开始。按迟到丢掉的话播放时钟永远
/// 追不回来，整场就静音了，所以这种情况直接重对齐。
static const uint64_t kAudioResetBehindUs = 8 * kAudioFrameUs;
static uint64_t g_play_next_ts = 0;
static int g_play_have_ts = 0;

static BOOL schedule_pcm(AVAudioPCMBuffer *pcm) {
    if (!pcm || pcm.frameLength == 0 || atomic_load(&g_play_scheduled) >= kAudioMaxScheduled) {
        return NO;
    }
    int gen = atomic_load(&g_play_gen);
    atomic_fetch_add(&g_play_scheduled, 1);
    [g_play_node scheduleBuffer:pcm completionHandler:^{
        if (atomic_load(&g_play_gen) == gen) {
            atomic_fetch_sub(&g_play_scheduled, 1);
        }
    }];
    if (!g_play_node.isPlaying) {
        [g_play_node play];
    }
    return YES;
}

/// 丢掉已经排队的旧声音，把播放时钟拨到当前包。只允许在播放队列上调用。
static void flush_playback(uint64_t ts) {
    atomic_fetch_add(&g_play_gen, 1);
    atomic_store(&g_play_scheduled, 0);
    if (g_play_node) {
        [g_play_node stop];
    }
    g_play_next_ts = ts;
    g_play_have_ts = 1;
}

/// 网络丢一帧时补 1024 个静音采样，保持播放时钟不被拉短。
static BOOL schedule_silence(void) {
    AVAudioPCMBuffer *pcm = [[AVAudioPCMBuffer alloc] initWithPCMFormat:g_play_pcm frameCapacity:1024];
    if (!pcm) {
        return NO;
    }
    pcm.frameLength = 1024;
    AudioBufferList *abl = pcm.mutableAudioBufferList;
    for (UInt32 i = 0; i < abl->mNumberBuffers; i++) {
        memset(abl->mBuffers[i].mData, 0, abl->mBuffers[i].mDataByteSize);
    }
    return schedule_pcm(pcm);
}

static void audio_play_init(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        g_play_q = dispatch_queue_create("jrp.audio.play", DISPATCH_QUEUE_SERIAL);
    });
}

/// 默认输出设备切换会让 AVAudioEngine 停转并清空已排队的缓冲。重新启动
/// 引擎，并把播放时钟交给下一包重新对齐；否则队列计数会一直停在旧值，
/// 之后每一包都排不进一个已经停掉的引擎。
static void play_engine_config_changed(void) {
    if (!g_play_engine) {
        return;
    }
    if (!g_play_engine.isRunning) {
        NSError *err = nil;
        if (![g_play_engine startAndReturnError:&err]) {
            NSLog(@"Jaryn Remote: audio engine restart failed: %@", err);
            // 设备切换时引擎可能一时起不来。留着这个停转的引擎，后面的包
            // 会全部排进一个永远不会响的节点，队列计数也会卡在满值，整场
            // 都恢复不了。拆掉这条播放链路，下一包按当前设备重建；重建再
            // 失败会置 g_play_failed，不会反复重试。
            [g_play_node stop];
            if (g_play_obs) {
                [[NSNotificationCenter defaultCenter] removeObserver:g_play_obs];
                g_play_obs = nil;
            }
            g_play_node = nil;
            g_play_engine = nil;
            g_play_dec = nil;
            g_play_aac = nil;
            g_play_pcm = nil;
            atomic_fetch_add(&g_play_gen, 1);
            atomic_store(&g_play_scheduled, 0);
            g_play_have_ts = 0;
            g_play_next_ts = 0;
            return;
        }
    }
    flush_playback(0);
    g_play_have_ts = 0;
}

static void audio_play_on_queue(NSData *data, uint64_t ts) {
    if (atomic_load(&g_play_failed) || data.length == 0) {
        return;
    }
    // 输出设备切换会先让引擎停转，通知可能还没轮到这条队列。这里每包确认
    // 一次引擎还在跑，否则停转期间排进去的缓冲和计数都白占。
    if (g_play_engine && !g_play_engine.isRunning) {
        play_engine_config_changed();
    }
    if (!g_play_engine) {
        // 协议固定 AAC-LC / 48 kHz / 双声道，裸帧没有 ADTS 头。解码器必须
        // 拿到 AudioSpecificConfig 才能建起来：objectType=2、频率索引=3、
        // 声道配置=2，拼成 13 bit 后是 0x11 0x90。
        static const uint8_t kAacAsc[2] = {0x11, 0x90};
        NSData *cookie = [NSData dataWithBytes:kAacAsc length:sizeof(kAacAsc)];
        NSDictionary *settings = @{
            AVFormatIDKey: @(kAudioFormatMPEG4AAC),
            AVSampleRateKey: @48000,
            AVNumberOfChannelsKey: @2,
        };
        g_play_aac = [[AVAudioFormat alloc] initWithSettings:settings];
        g_play_aac.magicCookie = cookie;
        g_play_pcm = [[AVAudioFormat alloc] initWithCommonFormat:AVAudioPCMFormatFloat32
                                                       sampleRate:48000
                                                         channels:2
                                                      interleaved:NO];
        g_play_dec = (g_play_aac && g_play_pcm) ? [[AVAudioConverter alloc] initFromFormat:g_play_aac toFormat:g_play_pcm] : nil;
        g_play_dec.magicCookie = cookie;
        AVAudioEngine *engine = [AVAudioEngine new];
        AVAudioPlayerNode *node = [AVAudioPlayerNode new];
        if (!g_play_dec || !engine || !node) {
            atomic_store(&g_play_failed, 1);
            return;
        }
        [engine attachNode:node];
        [engine connect:node to:engine.mainMixerNode format:g_play_pcm];
        NSError *err = nil;
        if (![engine startAndReturnError:&err]) {
            NSLog(@"Jaryn Remote: audio output unavailable: %@", err);
            atomic_store(&g_play_failed, 1);
            return;
        }
        g_play_engine = engine;
        g_play_node = node;
        if (g_play_obs) {
            [[NSNotificationCenter defaultCenter] removeObserver:g_play_obs];
        }
        g_play_obs = [[NSNotificationCenter defaultCenter]
            addObserverForName:AVAudioEngineConfigurationChangeNotification
                        object:engine
                         queue:nil
                    usingBlock:^(NSNotification *note) {
                        (void)note;
                        dispatch_async(g_play_q, ^{
                            play_engine_config_changed();
                        });
                    }];
    }
    if (atomic_load(&g_play_scheduled) >= kAudioMaxScheduled) {
        // 缓冲已经压满，继续排队只会让声音越来越落后。丢掉旧声音追当前包。
        flush_playback(ts);
    } else if (!g_play_have_ts) {
        g_play_next_ts = ts;
        g_play_have_ts = 1;
    } else {
        // 落后超过 80 ms 就整段丢掉重排；只落后一帧左右则丢这一包。
        if (ts > g_play_next_ts + 4 * kAudioFrameUs) {
            flush_playback(ts);
        } else if (ts + kAudioFrameUs < g_play_next_ts) {
            // 这里可能是乱序的迟到包，也可能是对端重开采集后时间戳回到 0。
            // 后者如果按迟到丢掉，g_play_next_ts 不会自己前进，之后每一包都
            // 被判成迟到，整个会话就再没有声音了；落后太多就当成时钟重启。
            if (g_play_next_ts - ts > kAudioResetBehindUs) {
                flush_playback(ts);
            } else {
                return;
            }
        } else {
            // 中间缺了几帧就补静音，最多补 4 帧；再长的空洞直接跳到当前时间。
            int filled = 0;
            while (g_play_next_ts + kAudioFrameUs < ts && filled < 4) {
                if (!schedule_silence()) {
                    break;
                }
                g_play_next_ts += kAudioFrameUs;
                filled++;
            }
        }
    }
    AVAudioCompressedBuffer *in = [[AVAudioCompressedBuffer alloc] initWithFormat:g_play_aac
                                                                  packetCapacity:1
                                                               maximumPacketSize:data.length];
    if (!in) {
        return;
    }
    memcpy(in.data, data.bytes, data.length);
    in.byteLength = (uint32_t)data.length;
    in.packetCount = 1;
    AudioStreamPacketDescription *pd = in.packetDescriptions;
    if (pd) {
        pd[0].mStartOffset = 0;
        pd[0].mDataByteSize = (UInt32)data.length;
        pd[0].mVariableFramesInPacket = 0;
    }
    AVAudioPCMBuffer *pcm = [[AVAudioPCMBuffer alloc] initWithPCMFormat:g_play_pcm frameCapacity:4096];
    if (!pcm) {
        return;
    }
    __block BOOL fed = NO;
    NSError *err = nil;
    AVAudioConverterOutputStatus status = [g_play_dec
        convertToBuffer:pcm
                 error:&err
        withInputFromBlock:^AVAudioBuffer *(AVAudioPacketCount inNumberOfPackets, AVAudioConverterInputStatus *outStatus) {
            if (fed) {
                *outStatus = AVAudioConverterInputStatus_NoDataNow;
                return nil;
            }
            fed = YES;
            *outStatus = AVAudioConverterInputStatus_HaveData;
            return in;
        }];
    if (status == AVAudioConverterOutputStatus_Error || pcm.frameLength == 0) {
        // 解码器建错（例如没有 magic cookie）时每帧都会失败；只报一次，
        // 免得把日志刷爆，但必须留下线索。
        if (!atomic_exchange(&g_play_warned, 1)) {
            NSLog(@"Jaryn Remote: AAC decode failed status=%ld frames=%u err=%@",
                  (long)status, (unsigned)pcm.frameLength, err);
        }
        return;
    }
    uint64_t at = MAX(g_play_next_ts, ts);
    if (schedule_pcm(pcm)) {
        g_play_next_ts = at + kAudioFrameUs;
    } else {
        // 补静音把最后一个槽占掉了。清空重来，保证真实声音不会永久排不进去。
        flush_playback(ts);
        if (schedule_pcm(pcm)) {
            g_play_next_ts = ts + kAudioFrameUs;
        }
    }
}

void jrp_audio_play(const uint8_t *bytes, int32_t len, uint64_t ts) {
    if (!bytes || len <= 0) {
        return;
    }
    NSData *data = [NSData dataWithBytes:bytes length:(NSUInteger)len];
    audio_play_init();
    dispatch_async(g_play_q, ^{
        audio_play_on_queue(data, ts);
    });
}

void jrp_audio_reset(void) {
    // 这个函数负责复位播放链路，但会话收尾时也会走到这里，所以顺带把采集
    // 侧整段收干净：和 jrp_audio_stop 一样在同一把锁下清回调和 g_audio_on，
    // 再释放 converter。AVAudioConverter 内部可能还留着上一段会话不足一包
    // 的 PCM，不能留给下一段会话复用。
    // 只释放 converter 而留下 g_audio_on 的话，编码线程等锁醒来会以为会话
    // 还在，重新建出 converter 并把旧 PCM 编出去；reset 自己也就不具备注释
    // 承诺的隔离性，只能靠调用方先 stop。这里一并清零，让 reset 单独调用
    // 也安全（现有调用方本来就先 stop，行为不变）。
    // 释放放在播放队列的 dispatch 之前单独持锁完成，锁内不碰播放状态，
    // 避免多把锁互相等待。
    pthread_mutex_lock(&g_audio_enc_mu);
    // 推进音频代次：同一代采集里 stop 之后可能马上又 start，只清 g_audio_on
    // 挡不住锁外还在排队的旧样本。
    atomic_fetch_add(&g_audio_gen, 1);
    atomic_store(&g_audio_on, 0);
    g_audio = NULL;
    g_audio_ctx = NULL;
    audio_enc_release_locked();
    pthread_mutex_unlock(&g_audio_enc_mu);
    audio_play_init();
    dispatch_async(g_play_q, ^{
        atomic_fetch_add(&g_play_gen, 1);
        [g_play_node stop];
        [g_play_engine stop];
        if (g_play_obs) {
            [[NSNotificationCenter defaultCenter] removeObserver:g_play_obs];
            g_play_obs = nil;
        }
        g_play_node = nil;
        g_play_engine = nil;
        g_play_dec = nil;
        g_play_aac = nil;
        g_play_pcm = nil;
        atomic_store(&g_play_scheduled, 0);
        atomic_store(&g_play_failed, 0);
        atomic_store(&g_play_warned, 0);
        g_play_have_ts = 0;
        g_play_next_ts = 0;
    });
}

static NSWindow *g_window;
static NSView *g_video;
static VTDecompressionSessionRef g_decode;
static CMFormatDescriptionRef g_fmt;
static CVPixelBufferRef g_latest;
static uint32_t g_view_w = 1280;
static uint32_t g_view_h = 720;
static bool g_modifier_down[128];

// 待发给主控的本地光标形状（协议编码：宽、高、热点、RGBA），以及主控送来的远端形状。
static pthread_mutex_t g_shape_mu = PTHREAD_MUTEX_INITIALIZER;
static uint8_t *g_local_shape;
static int g_local_shape_len;
static uint8_t *g_remote_shape;
static int g_remote_shape_len;
static atomic_int g_shape_req;
static atomic_int g_remote_shape_dirty;
static atomic_int g_cursor_dirty;
static atomic_int g_cursor_x;
static atomic_int g_cursor_y;
static atomic_int g_cursor_visible;
static atomic_int g_cursor_hot_x;
static atomic_int g_cursor_hot_y;
static NSImage *g_cursor_image;
static CFAbsoluteTime g_last_shape_at;

@interface JrpVideoView : NSView
{
    // 按下时按在了画面里没有。越界的那一下不发按下也不发松开，按钮才配得平。
    BOOL _left_in_picture;
    BOOL _right_in_picture;
    BOOL _middle_in_picture;
}
- (void)releasePointerButtons;
- (void)clearPointerButtons;
@end

/// 观看窗口顶部的工具条：和远端画面分开，不遮住画面的任何一行像素。
@interface JrpBar : NSView
@end

@implementation JrpBar
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    [[NSColor windowBackgroundColor] setFill];
    NSRectFill(self.bounds);
    // 非翻转坐标：y=0 就是贴着画面的那条边，细线画在那里把工具条和画面分开。
    [[[NSColor labelColor] colorWithAlphaComponent:0.12] setFill];
    NSRectFill(NSMakeRect(0, 0, self.bounds.size.width, 1));
}
@end

/// 画面区域：按远端分辨率等比缩放到视图内居中，多出来的部分留黑。
/// 鼠标坐标必须用同一个矩形反算，不然画面被缩放后鼠标会偏。
static NSRect jr_video_area(NSRect bounds) {
    if (g_view_w == 0 || g_view_h == 0) {
        return bounds;
    }
    double scale = MIN(bounds.size.width / (double)g_view_w, bounds.size.height / (double)g_view_h);
    if (!(scale > 0.0)) {
        return bounds;
    }
    CGFloat w = (CGFloat)g_view_w * (CGFloat)scale;
    CGFloat h = (CGFloat)g_view_h * (CGFloat)scale;
    return NSMakeRect(bounds.origin.x + (bounds.size.width - w) / 2.0,
                      bounds.origin.y + (bounds.size.height - h) / 2.0,
                      w, h);
}

@implementation JrpVideoView
- (BOOL)acceptsFirstResponder {
    return YES;
}
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    [[NSColor blackColor] setFill];
    NSRectFill(self.bounds);
    pthread_mutex_lock(&g_mu);
    CVPixelBufferRef frame = g_latest ? CVPixelBufferRetain(g_latest) : NULL;
    pthread_mutex_unlock(&g_mu);
    if (!frame) {
        // 还没有能解码的画面：说清楚在等，不要只给一块黑。
        NSMutableParagraphStyle *style = [NSMutableParagraphStyle new];
        style.alignment = NSTextAlignmentCenter;
        NSAttributedString *hint = [[NSAttributedString alloc] initWithString:@"等待远端画面…" attributes:@{
            NSFontAttributeName: [NSFont systemFontOfSize:14 weight:NSFontWeightMedium],
            NSForegroundColorAttributeName: [[NSColor whiteColor] colorWithAlphaComponent:0.42],
            NSParagraphStyleAttributeName: style,
        }];
        [hint drawInRect:NSMakeRect(0, NSMidY(self.bounds) - 10, self.bounds.size.width, 20)];
        return;
    }
    CIImage *image = [CIImage imageWithCVPixelBuffer:frame];
    CIContext *ctx = [CIContext context];
    [ctx drawImage:image inRect:jr_video_area(self.bounds) fromRect:image.extent];
    CVPixelBufferRelease(frame);
    [self drawRemoteCursor];
}
/// 远端光标画在自己的热点上。形状没到之前不画，别用本机箭头冒充。
- (void)drawRemoteCursor {
    if (!atomic_load(&g_cursor_visible) || !g_cursor_image || g_view_w == 0 || g_view_h == 0) {
        return;
    }
    NSRect area = jr_video_area(self.bounds);
    if (area.size.width <= 0 || area.size.height <= 0) {
        return;
    }
    double sx = area.size.width / (double)g_view_w;
    double sy = area.size.height / (double)g_view_h;
    NSSize size = g_cursor_image.size;
    int32_t x = atomic_load(&g_cursor_x);
    int32_t y = atomic_load(&g_cursor_y);
    int16_t hot_x = (int16_t)atomic_load(&g_cursor_hot_x);
    int16_t hot_y = (int16_t)atomic_load(&g_cursor_hot_y);
    CGFloat left = area.origin.x + (CGFloat)(x - hot_x) * sx;
    CGFloat bottom = area.origin.y + area.size.height - (CGFloat)(y - hot_y) * sy - size.height * sy;
    NSRect at = NSMakeRect(left, bottom, size.width * sx, size.height * sy);
    [g_cursor_image drawInRect:at
                      fromRect:NSZeroRect
                     operation:NSCompositingOperationSourceOver
                      fraction:1.0
                respectFlipped:NO
                         hints:nil];
}
/// 回传鼠标位置；点落在画面外的留边上时什么也不发，并回 NO。
- (BOOL)sendMove:(NSEvent *)event {
    if (!g_input || ![g_window isKeyWindow] || g_view_w == 0 || g_view_h == 0) {
        return NO;
    }
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    NSRect area = jr_video_area(self.bounds);
    // 留边上的点击不算远端画面，不要发过去。
    if (area.size.width <= 0 || area.size.height <= 0 || !NSPointInRect(p, area)) {
        return NO;
    }
    int32_t x = (int32_t)((p.x - area.origin.x) / area.size.width * g_view_w);
    int32_t y = (int32_t)((1.0 - (p.y - area.origin.y) / area.size.height) * g_view_h);
    if (x < 0) x = 0;
    if (y < 0) y = 0;
    if ((uint32_t)x >= g_view_w) x = (int32_t)g_view_w - 1;
    if ((uint32_t)y >= g_view_h) y = (int32_t)g_view_h - 1;
    g_input(1, x, y, 0, g_input_ctx);
    return YES;
}
- (void)mouseMoved:(NSEvent *)event {
    [self sendMove:event];
}
- (void)mouseDragged:(NSEvent *)event {
    [self sendMove:event];
}
- (void)mouseDown:(NSEvent *)event {
    _left_in_picture = [self sendMove:event];
    if (_left_in_picture) {
        [self sendButton:0 down:1];
    }
}
- (void)mouseUp:(NSEvent *)event {
    // 按下落在了画面外就不补发松开，否则远端会收到一次没人按下的按键。
    if (_left_in_picture) {
        [self sendButton:0 down:0];
    }
    _left_in_picture = NO;
}
- (void)scrollWheel:(NSEvent *)event {
    if (g_input && [g_window isKeyWindow]) {
        double dy = event.scrollingDeltaY;
        double dx = event.scrollingDeltaX;
        if (event.hasPreciseScrollingDeltas) {
            dy *= 4.0;
            dx *= 4.0;
        }
        g_input(4, (int32_t)dy, (int32_t)dx, 0, g_input_ctx);
    }
}
- (void)rightMouseDown:(NSEvent *)event {
    _right_in_picture = [self sendMove:event];
    if (_right_in_picture) {
        [self sendButton:1 down:1];
    }
}
- (void)rightMouseUp:(NSEvent *)event {
    if (_right_in_picture) {
        [self sendButton:1 down:0];
    }
    _right_in_picture = NO;
}
- (void)otherMouseDown:(NSEvent *)event {
    _middle_in_picture = [self sendMove:event];
    if (_middle_in_picture) {
        [self sendButton:2 down:1];
    }
}
- (void)otherMouseUp:(NSEvent *)event {
    if (_middle_in_picture) {
        [self sendButton:2 down:0];
    }
    _middle_in_picture = NO;
}
/// 窗口失焦或关闭时补发按下中的鼠标键。此时已经不满足 isKeyWindow，
/// 不能再走 sendButton 的焦点检查，否则远端会一直以为键还按着。
- (void)releasePointerButtons {
    if (!g_input) {
        [self clearPointerButtons];
        return;
    }
    if (_left_in_picture) {
        g_input(2, 0, 0, 0, g_input_ctx);
    }
    if (_right_in_picture) {
        g_input(2, 1, 0, 0, g_input_ctx);
    }
    if (_middle_in_picture) {
        g_input(2, 2, 0, 0, g_input_ctx);
    }
    [self clearPointerButtons];
}
/// 只清本地的按下标记，不发事件。会话已经结束时不往对面补发。
- (void)clearPointerButtons {
    _left_in_picture = NO;
    _right_in_picture = NO;
    _middle_in_picture = NO;
}
- (void)keyDown:(NSEvent *)event {
    if (event.isARepeat) {
        return;
    }
    if (g_input && [g_window isKeyWindow]) {
        g_input(3, event.keyCode, 1, 0, g_input_ctx);
    }
}
- (void)keyUp:(NSEvent *)event {
    if (g_input && [g_window isKeyWindow]) {
        g_input(3, event.keyCode, 0, 0, g_input_ctx);
    }
}
- (void)flagsChanged:(NSEvent *)event {
    if (!g_input || ![g_window isKeyWindow] || event.keyCode >= 128) {
        return;
    }
    switch (event.keyCode) {
        case 0x36: case 0x37: // Command
        case 0x38: case 0x3C: // Shift
        case 0x3A: case 0x3D: // Option
        case 0x3B: case 0x3E: // Control
            g_modifier_down[event.keyCode] = !g_modifier_down[event.keyCode];
            g_input(3, event.keyCode, g_modifier_down[event.keyCode] ? 1 : 0, 0, g_input_ctx);
            break;
        default:
            break;
    }
}
- (void)sendButton:(int)button down:(int)down {
    if (g_input && [g_window isKeyWindow]) {
        g_input(2, button, down, 0, g_input_ctx);
    }
}
@end

static void release_view_modifiers(void) {
    for (int code = 0; code < 128; code++) {
        if (g_modifier_down[code]) {
            g_modifier_down[code] = false;
            if (g_input) g_input(3, code, 0, 0, g_input_ctx);
        }
    }
}

@interface JrpWin : NSObject <NSWindowDelegate>
@end

@implementation JrpWin
- (void)pickQuality:(id)sender {
    NSPopUpButton *pop = (NSPopUpButton *)sender;
    if (g_menu) {
        g_menu(1, (int32_t)pop.indexOfSelectedItem + 1, g_menu_ctx);
    }
}
- (void)pickDisplay:(id)sender {
    NSPopUpButton *pop = (NSPopUpButton *)sender;
    if (g_menu) {
        g_menu(2, (int32_t)pop.indexOfSelectedItem, g_menu_ctx);
    }
}
- (void)windowDidBecomeKey:(NSNotification *)note {
    (void)note;
    atomic_store(&g_focused, 1);
}
- (void)windowDidResignKey:(NSNotification *)note {
    (void)note;
    atomic_store(&g_focused, 0);
    release_view_modifiers();
    [(JrpVideoView *)g_video releasePointerButtons];
}
- (void)windowWillClose:(NSNotification *)note {
    (void)note;
    atomic_store(&g_focused, 0);
    release_view_modifiers();
    [(JrpVideoView *)g_video releasePointerButtons];
    atomic_store(&g_hangup, 1);
}
- (void)hangup:(id)sender {
    (void)sender;
    atomic_store(&g_hangup, 1);
}
@end

static JrpWin *g_delegate;
static NSStatusItem *g_status;

static void decode_cb(void *refcon, void *source, OSStatus status, VTDecodeInfoFlags flags, CVImageBufferRef image, CMTime pts, CMTime duration) {
    (void)refcon;
    (void)source;
    (void)flags;
    (void)pts;
    (void)duration;
    if (status != noErr || !image) {
        atomic_store(&g_need_keyframe, 1);
        return;
    }
    pthread_mutex_lock(&g_mu);
    if (g_latest) {
        CVPixelBufferRelease(g_latest);
    }
    g_latest = CVPixelBufferRetain(image);
    pthread_mutex_unlock(&g_mu);
    dispatch_async(dispatch_get_main_queue(), ^{
        [g_video setNeedsDisplay:YES];
    });
}

static int nal_type(const uint8_t *p) {
    return p[0] & 0x1f;
}

static void present_annex(const uint8_t *bytes, int32_t len, uint8_t idr) {
    if (!bytes || len < 5) {
        return;
    }
    const uint8_t *sps = NULL;
    const uint8_t *pps = NULL;
    size_t sps_len = 0;
    size_t pps_len = 0;
    const uint8_t *frame = NULL;
    size_t frame_len = 0;
    int32_t i = 0;
    while (i + 4 < len) {
        int sc = 0;
        if (bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1) {
            sc = 3;
        } else if (bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1) {
            sc = 4;
        }
        if (!sc) {
            i++;
            continue;
        }
        int start = i + sc;
        int next = start;
        while (next + 3 < len) {
            if (bytes[next] == 0 && bytes[next + 1] == 0 && (bytes[next + 2] == 1 || (bytes[next + 2] == 0 && next + 3 < len && bytes[next + 3] == 1))) {
                break;
            }
            next++;
        }
        if (next + 3 >= len) {
            next = len;
        }
        int ntype = nal_type(bytes + start);
        if (ntype == 7) {
            sps = bytes + start;
            sps_len = (size_t)(next - start);
        } else if (ntype == 8) {
            pps = bytes + start;
            pps_len = (size_t)(next - start);
        } else if (ntype == 1 || ntype == 5) {
            frame = bytes + start;
            frame_len = (size_t)(next - start);
        }
        i = next;
    }
    if (idr && sps && pps) {
        if (g_decode) {
            VTDecompressionSessionInvalidate(g_decode);
            CFRelease(g_decode);
            g_decode = NULL;
        }
        if (g_fmt) {
            CFRelease(g_fmt);
            g_fmt = NULL;
        }
        const uint8_t *sets[2] = {sps, pps};
        size_t sizes[2] = {sps_len, pps_len};
        if (CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, 2, sets, sizes, 4, &g_fmt) != noErr) {
            return;
        }
        VTDecompressionOutputCallbackRecord rec = {.decompressionOutputCallback = decode_cb, .decompressionOutputRefCon = NULL};
        if (VTDecompressionSessionCreate(NULL, g_fmt, NULL, NULL, &rec, &g_decode) != noErr) {
            g_decode = NULL;
        }
    }
    if (!g_decode || !g_fmt || !frame || frame_len == 0) {
        atomic_store(&g_need_keyframe, 1);
        return;
    }
    uint32_t nlen = CFSwapInt32HostToBig((uint32_t)frame_len);
    size_t packed = 4 + frame_len;
    uint8_t *buf = malloc(packed);
    if (!buf) {
        return;
    }
    memcpy(buf, &nlen, 4);
    memcpy(buf + 4, frame, frame_len);
    CMBlockBufferRef block = NULL;
    if (CMBlockBufferCreateWithMemoryBlock(NULL, buf, packed, kCFAllocatorMalloc, NULL, 0, packed, 0, &block) != kCMBlockBufferNoErr) {
        free(buf);
        return;
    }
    CMSampleBufferRef sample = NULL;
    CMSampleTimingInfo timing = {.duration = kCMTimeInvalid, .presentationTimeStamp = CMTimeMake((int64_t)(CACurrentMediaTime() * 1000.0), 1000), .decodeTimeStamp = kCMTimeInvalid};
    size_t size_one = packed;
    OSStatus sb = CMSampleBufferCreate(NULL, block, true, NULL, NULL, g_fmt, 1, 1, &timing, 1, &size_one, &sample);
    CFRelease(block);
    if (sb != noErr || !sample) {
        return;
    }
    VTDecodeInfoFlags info = 0;
    if (VTDecompressionSessionDecodeFrame(g_decode, sample, 0, NULL, &info) != noErr) {
        atomic_store(&g_need_keyframe, 1);
    }
    CFRelease(sample);
}

/// 画质请求只在窗口存在时消费；窗口没开就先留着，开窗后补上。
static void apply_quality_on_main(void) {
    if (!g_quality_pop) {
        return;
    }
    int profile = atomic_exchange(&g_quality_req, 0);
    if (profile >= 1 && profile <= 4 && profile - 1 < (int)g_quality_pop.numberOfItems) {
        [g_quality_pop selectItemAtIndex:profile - 1];
    }
}

/// 按最新显示器列表重建下拉框，尽量保留用户当前选择。
static void apply_displays_on_main(void) {
    if (!g_display_pop || !atomic_exchange(&g_displays_req, 0)) {
        return;
    }
    pthread_mutex_lock(&g_display_mu);
    NSString *packed = g_display_names ? @(g_display_names) : @"";
    pthread_mutex_unlock(&g_display_mu);
    NSArray<NSString *> *names = [packed componentsSeparatedByString:@"\n"];
    NSInteger old = g_display_pop.indexOfSelectedItem;
    int req = atomic_exchange(&g_display_sel_req, -1);
    if (req >= 0) {
        old = req;
    }
    [g_display_pop removeAllItems];
    for (NSString *name in names) {
        if (name.length > 0) {
            [g_display_pop addItemWithTitle:name];
        }
    }
    if (g_display_pop.numberOfItems > 0) {
        NSInteger pick = MIN(MAX(old, 0), (NSInteger)g_display_pop.numberOfItems - 1);
        [g_display_pop selectItemAtIndex:pick];
    }
}

static void open_view_on_main(void) {
    if (g_window) {
        // 复用同一扇窗：远端换了分辨率、或者换了另一台机器时，
        // 等比留边的比例和鼠标坐标换算都得跟着更新，不然会拉伸和错位。
        if (g_req_w) g_view_w = g_req_w;
        if (g_req_h) g_view_h = g_req_h;
        [g_video setNeedsDisplay:YES];
        [g_window makeKeyAndOrderFront:nil];
        return;
    }
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
    g_view_w = g_req_w ? g_req_w : 1280;
    g_view_h = g_req_h ? g_req_h : 720;
    NSRect frame = NSMakeRect(80, 80, 960, 600);
    g_window = [[NSWindow alloc] initWithContentRect:frame
                                           styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable | NSWindowStyleMaskMiniaturizable
                                             backing:NSBackingStoreBuffered
                                               defer:NO];
    // 窗口是长期复用的，交给 ARC 管，不要被 AppKit 再释放一次。
    g_window.releasedWhenClosed = NO;
    g_window.title = @"Jaryn Remote · 远程画面";
    g_window.minSize = NSMakeSize(720, 420);
    g_delegate = [JrpWin new];
    g_window.delegate = g_delegate;
    NSView *content = g_window.contentView;
    CGFloat cw = content.bounds.size.width;
    CGFloat ch = content.bounds.size.height;

    // 顶栏：左边是画质和显示器，右边是加密说明和断开。
    JrpBar *bar = [[JrpBar alloc] initWithFrame:NSMakeRect(0, ch - 44, cw, 44)];
    bar.autoresizingMask = NSViewWidthSizable | NSViewMinYMargin;
    [content addSubview:bar];

    NSTextField *quality_label = [NSTextField labelWithString:@"画质"];
    quality_label.font = [NSFont systemFontOfSize:11.5 weight:NSFontWeightMedium];
    quality_label.textColor = [NSColor tertiaryLabelColor];
    quality_label.frame = NSMakeRect(16, 14, 32, 16);
    [bar addSubview:quality_label];

    NSPopUpButton *quality = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(50, 10, 104, 26) pullsDown:NO];
    [quality addItemsWithTitles:@[@"清晰", @"均衡", @"高帧率", @"省流量"]];
    [quality selectItemAtIndex:1];
    quality.controlSize = NSControlSizeSmall;
    quality.font = [NSFont systemFontOfSize:11.5];
    quality.target = g_delegate;
    quality.action = @selector(pickQuality:);
    quality.autoresizingMask = NSViewMinYMargin;
    [bar addSubview:quality];
    g_quality_pop = quality;

    NSTextField *display_label = [NSTextField labelWithString:@"显示器"];
    display_label.font = [NSFont systemFontOfSize:11.5 weight:NSFontWeightMedium];
    display_label.textColor = [NSColor tertiaryLabelColor];
    display_label.frame = NSMakeRect(166, 14, 44, 16);
    [bar addSubview:display_label];

    NSPopUpButton *display = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(212, 10, 150, 26) pullsDown:NO];
    display.controlSize = NSControlSizeSmall;
    display.font = [NSFont systemFontOfSize:11.5];
    display.target = g_delegate;
    display.action = @selector(pickDisplay:);
    display.autoresizingMask = NSViewMinYMargin;
    [bar addSubview:display];
    g_display_pop = display;
    atomic_store(&g_displays_req, 1);

    NSTextField *secure = [NSTextField labelWithString:@"端到端加密会话"];
    secure.font = [NSFont systemFontOfSize:11 weight:NSFontWeightRegular];
    secure.textColor = [NSColor secondaryLabelColor];
    secure.alignment = NSTextAlignmentRight;
    secure.frame = NSMakeRect(cw - 16 - 76 - 12 - 160, 14, 160, 16);
    secure.autoresizingMask = NSViewMinXMargin | NSViewMinYMargin;
    [bar addSubview:secure];

    NSButton *end = [NSButton buttonWithTitle:@"断开" target:g_delegate action:@selector(hangup:)];
    end.frame = NSMakeRect(cw - 16 - 76, 9, 76, 26);
    end.bezelStyle = NSBezelStyleRounded;
    end.controlSize = NSControlSizeSmall;
    end.font = [NSFont systemFontOfSize:12 weight:NSFontWeightMedium];
    end.toolTip = @"结束这次远控（对方也会看到会话结束）";
    end.autoresizingMask = NSViewMinXMargin | NSViewMinYMargin;
    [bar addSubview:end];

    JrpVideoView *video = [JrpVideoView new];
    video.frame = NSMakeRect(0, 0, cw, ch - 44);
    video.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [content addSubview:video];
    g_video = video;
    [g_window setAcceptsMouseMovedEvents:YES];
    [g_window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
}

/// 遮挡层的强调色：和产品图标里那笔青绿色的 J 同一个色。
static NSColor *cover_accent(void) {
    return [NSColor colorWithSRGBRed:0.19 green:0.78 blue:0.93 alpha:1.0];
}

/// 每一块屏一层遮挡。只盖主屏等于没盖，所以逐屏各建一个无边框窗口。
static NSMutableArray<NSWindow *> *g_covers;
/// 遮挡当前是不是该开着。屏幕增减时用它决定要不要重建。
static BOOL g_cover_wanted = NO;
/// NSApplicationDidChangeScreenParametersNotification 的登记号。
static id g_cover_screen_token;
/// 跨线程读的开头状态。窗口只能在主线程建，但这一个值可以被别的线程问。
static atomic_int g_cover_state = 0;

/// 遮挡层内容：品牌深蓝底 + 一句话实话。
static void cover_fill_screen(NSWindow *win, NSRect frame) {
    CGFloat w = frame.size.width;
    CGFloat h = frame.size.height;
    CGFloat cy = h / 2;
    NSView *content = win.contentView;

    // 用 NSBox 而不是 layer 背景：不引入 QuartzCore，也跟随系统外观。
    // NSBoxCustom 默认带 1pt 边框，不关掉这 4pt 高的短条就只剩边框了。
    NSBox *accent = [[NSBox alloc] initWithFrame:NSMakeRect(w / 2 - 24, cy + 74, 48, 4)];
    accent.boxType = NSBoxCustom;
    accent.borderWidth = 0;
    accent.fillColor = cover_accent();
    accent.cornerRadius = 2;
    [content addSubview:accent];

    NSTextField *title = [NSTextField labelWithString:@"正在被远控"];
    title.alignment = NSTextAlignmentCenter;
    title.textColor = [NSColor whiteColor];
    title.font = [NSFont systemFontOfSize:30 weight:NSFontWeightSemibold];
    title.frame = NSMakeRect(40, cy + 22, MAX(120, w - 80), 38);
    [content addSubview:title];

    CGFloat body_w = MIN(660, MAX(160, w - 80));
    NSTextField *body = [NSTextField wrappingLabelWithString:@"屏幕内容已遮挡。这是软件遮挡，不是硬件隐私屏：它挡不住截图或录屏。"];
    body.alignment = NSTextAlignmentCenter;
    body.textColor = [[NSColor whiteColor] colorWithAlphaComponent:0.72];
    body.font = [NSFont systemFontOfSize:15 weight:NSFontWeightRegular];
    body.frame = NSMakeRect((w - body_w) / 2, cy - 44, body_w, 46);
    [content addSubview:body];

    NSTextField *foot = [NSTextField labelWithString:@"Jaryn Remote"];
    foot.alignment = NSTextAlignmentCenter;
    foot.textColor = [[NSColor whiteColor] colorWithAlphaComponent:0.42];
    foot.font = [NSFont systemFontOfSize:12 weight:NSFontWeightMedium];
    foot.frame = NSMakeRect(40, 40, MAX(120, w - 80), 18);
    [content addSubview:foot];
}

/// 软件遮挡层：盖住每一块屏，写清楚这不是硬件隐私屏。
/// macOS 没有公开的虚拟显示器接口，所以只能做到这一步，并且要如实告知。
static void cover_build(void) {
    if (g_covers.count > 0) {
        return;
    }
    [NSApplication sharedApplication];
    if (NSApp.activationPolicy == NSApplicationActivationPolicyProhibited) {
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    }
    g_covers = [NSMutableArray array];
    NSArray<NSScreen *> *screens = NSScreen.screens.count ? NSScreen.screens : (NSScreen.mainScreen ? @[NSScreen.mainScreen] : @[]);
    for (NSScreen *screen in screens) {
        NSRect frame = screen.frame;
        NSWindow *win = [[NSWindow alloc] initWithContentRect:frame
                                                    styleMask:NSWindowStyleMaskBorderless
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO
                                                       screen:screen];
        win.releasedWhenClosed = NO;
        win.level = NSScreenSaverWindowLevel;
        win.opaque = YES;
        // 品牌深蓝（取自图标底色），比纯黑更像一个刻意显示的状态，而不是死机。
        win.backgroundColor = [NSColor colorWithSRGBRed:0.043 green:0.090 blue:0.220 alpha:1.0];
        win.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorStationary;
        cover_fill_screen(win, frame);
        [g_covers addObject:win];
        [win orderFrontRegardless];
    }
    atomic_store(&g_cover_state, g_covers.count > 0 ? 1 : 0);
}

static void cover_release(void) {
    for (NSWindow *win in g_covers) {
        [win orderOut:nil];
    }
    g_covers = nil;
    atomic_store(&g_cover_state, 0);
}

static void cover_on_main(int on) {
    if (on) {
        g_cover_wanted = YES;
        if (!g_cover_screen_token) {
            // 插拔显示器时按新的屏幕集合重建，否则会留下没盖住的屏。
            g_cover_screen_token = [NSNotificationCenter.defaultCenter
                addObserverForName:NSApplicationDidChangeScreenParametersNotification
                            object:nil
                             queue:NSOperationQueue.mainQueue
                        usingBlock:^(NSNotification *note) {
                            (void)note;
                            if (!g_cover_wanted) {
                                return;
                            }
                            cover_release();
                            cover_build();
                        }];
        }
        cover_build();
        return;
    }
    g_cover_wanted = NO;
    cover_release();
    if (g_cover_screen_token) {
        [NSNotificationCenter.defaultCenter removeObserver:g_cover_screen_token];
        g_cover_screen_token = nil;
    }
}

void jrp_privacy_cover(int on) {
    atomic_store(&g_cover_req, on ? 1 : 2);
}

int jrp_privacy_cover_on(void) {
    // Rust 侧按 == 1 判断，多屏时窗口不止一个，所以这里仍只回 0/1。
    return atomic_load(&g_cover_state);
}

/// 把文本放进系统剪贴板。对面按下「发剪贴板」的时候才会走到这里，
/// 不是后台偷偷同步。
void jrp_clipboard_set(const char *utf8, int len) {
    if (!utf8 || len <= 0) {
        return;
    }
    NSString *text = [[NSString alloc] initWithBytes:utf8 length:(NSUInteger)len encoding:NSUTF8StringEncoding];
    if (!text) {
        return;
    }
    NSPasteboard *pb = [NSPasteboard generalPasteboard];
    [pb clearContents];
    [pb setString:text forType:NSPasteboardTypeString];
}

/// 读系统剪贴板里的文本。返回长度，写进 buf；不是文本或放不下就返回 0。
int jrp_clipboard_get(char *buf, int cap) {
    if (!buf || cap <= 0) {
        return 0;
    }
    NSString *text = [[NSPasteboard generalPasteboard] stringForType:NSPasteboardTypeString];
    if (!text) {
        return 0;
    }
    NSData *data = [text dataUsingEncoding:NSUTF8StringEncoding];
    if (!data || data.length > (NSUInteger)cap) {
        return 0;
    }
    memcpy(buf, data.bytes, data.length);
    return (int)data.length;
}

static void indicator_on_main(int on) {
    if (!on) {
        if (g_status) {
            [[NSStatusBar systemStatusBar] removeStatusItem:g_status];
            g_status = nil;
        }
        return;
    }
    if (g_status) {
        return;
    }
    [NSApplication sharedApplication];
    if (NSApp.activationPolicy == NSApplicationActivationPolicyProhibited) {
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    }
    g_status = [[NSStatusBar systemStatusBar] statusItemWithLength:NSVariableStatusItemLength];
    g_status.button.title = @"正在被控";
    if (!g_delegate) {
        g_delegate = [JrpWin new];
    }
    NSMenu *menu = [NSMenu new];
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:@"断开" action:@selector(hangup:) keyEquivalent:@""];
    item.target = g_delegate;
    [menu addItem:item];
    g_status.menu = menu;
}

void jrp_view_request_open(uint32_t width, uint32_t height) {
    if (width) g_req_w = width;
    if (height) g_req_h = height;
    atomic_store(&g_open_req, 1);
}

void jrp_view_present(const uint8_t *bytes, int32_t len, uint8_t idr) {
    if (!bytes || len <= 0) {
        return;
    }
    JrpPendingFrame *item = calloc(1, sizeof(JrpPendingFrame));
    if (!item) return;
    item->bytes = malloc((size_t)len);
    if (!item->bytes) { free(item); return; }
    memcpy(item->bytes, bytes, (size_t)len);
    item->len = len;
    item->idr = idr;
    pthread_mutex_lock(&g_mu);
    if (idr || g_pending_count >= 32) {
        while (g_pending_head) {
            JrpPendingFrame *old = g_pending_head;
            g_pending_head = old->next;
            free(old->bytes);
            free(old);
        }
        g_pending_tail = NULL;
        g_pending_count = 0;
        if (!idr) {
            g_wait_for_idr = true;
            atomic_store(&g_need_keyframe, 1);
        }
    }
    if (g_wait_for_idr && !idr) {
        // 首帧 IDR 丢了时，不能只是把后续帧全丢掉；不主动要关键帧，
        // 主控窗口会一直黑着。
        atomic_store(&g_need_keyframe, 1);
        pthread_mutex_unlock(&g_mu);
        free(item->bytes);
        free(item);
        return;
    }
    if (idr) g_wait_for_idr = false;
    if (g_pending_tail) g_pending_tail->next = item;
    else g_pending_head = item;
    g_pending_tail = item;
    g_pending_count++;
    pthread_mutex_unlock(&g_mu);
}

int jrp_view_take_need_keyframe(void) {
    return atomic_exchange(&g_need_keyframe, 0);
}

/// 会话结束：丢掉上一段的待解码帧并隐藏远端光标，下一段重新等 IDR。
void jrp_view_reset(void) {
    pthread_mutex_lock(&g_mu);
    while (g_pending_head) {
        JrpPendingFrame *old = g_pending_head;
        g_pending_head = old->next;
        free(old->bytes);
        free(old);
    }
    g_pending_tail = NULL;
    g_pending_count = 0;
    g_wait_for_idr = true;
    atomic_store(&g_need_keyframe, 0);
    pthread_mutex_unlock(&g_mu);
    atomic_store(&g_cursor_visible, 0);
    atomic_store(&g_cursor_dirty, 1);
    // 会话结束也把“按下中”的本地标记清掉。窗口下一段会话还在，留着旧标记
    // 会让下一次按下前的松开逻辑判断错。ivars 只由主线程改。
    dispatch_async(dispatch_get_main_queue(), ^{
        if (g_video) {
            [(JrpVideoView *)g_video clearPointerButtons];
        }
    });
}

void jrp_view_set_input(jrp_input_cb cb, void *ctx) {
    g_input = cb;
    g_input_ctx = ctx;
}

void jrp_view_set_menu(jrp_menu_cb cb, void *ctx) {
    g_menu = cb;
    g_menu_ctx = ctx;
}

/// 显示器名字按行分开。窗口还没开时先存着，开窗时填进下拉框；
/// 已经开着就直接触发重建，保证会话中增减显示器能立刻看到。
void jrp_view_set_displays(const char *packed) {
    pthread_mutex_lock(&g_display_mu);
    free(g_display_names);
    g_display_names = NULL;
    if (packed) {
        size_t n = strlen(packed);
        g_display_names = malloc(n + 1);
        if (g_display_names) {
            memcpy(g_display_names, packed, n + 1);
        }
    }
    pthread_mutex_unlock(&g_display_mu);
    atomic_store(&g_displays_req, 1);
}

void jrp_view_set_quality(int profile) {
    if (profile < 1 || profile > 4) {
        return;
    }
    atomic_store(&g_quality_req, profile);
}

void jrp_view_set_display(int index) {
    atomic_store(&g_display_sel_req, index < 0 ? 0 : index);
}

int jrp_view_focused(void) {
    return atomic_load(&g_focused);
}

void jrp_indicator(int on) {
    atomic_store(&g_indicator_req, on ? 1 : 2);
}

int jrp_take_hangup(void) {
    return atomic_exchange(&g_hangup, 0);
}

void jrp_cursor(int32_t *x, int32_t *y) {
    CGEventRef event = CGEventCreate(NULL);
    if (!event) {
        *x = 0;
        *y = 0;
        return;
    }
    CGPoint p = CGEventGetLocation(event);
    CFRelease(event);
    *x = (int32_t)p.x;
    *y = (int32_t)p.y;
}

/// 主线程抓一次系统光标，编码成和协议相同的 16 字节头 + BGRA。
/// 形状没变也照抓，去重交给 Rust 那边按内容做。
static uint64_t shape_hash(uint16_t hx, uint16_t hy, uint16_t w, uint16_t h, const uint8_t *px, size_t n) {
    uint64_t v = 1469598103934665603ULL;
    uint8_t head[8] = {
        (uint8_t)(hx & 0xff), (uint8_t)(hx >> 8),
        (uint8_t)(hy & 0xff), (uint8_t)(hy >> 8),
        (uint8_t)(w & 0xff), (uint8_t)(w >> 8),
        (uint8_t)(h & 0xff), (uint8_t)(h >> 8),
    };
    for (size_t i = 0; i < sizeof(head); i++) {
        v ^= head[i];
        v *= 1099511628211ULL;
    }
    for (size_t i = 0; i < n; i++) {
        v ^= px[i];
        v *= 1099511628211ULL;
    }
    return v;
}

static void put_u16(uint8_t *p, uint16_t v) {
    p[0] = (uint8_t)(v & 0xff);
    p[1] = (uint8_t)(v >> 8);
}

static void put_u64(uint8_t *p, uint64_t v) {
    for (int i = 0; i < 8; i++) {
        p[i] = (uint8_t)((v >> (i * 8)) & 0xff);
    }
}

static void capture_cursor_shape_on_main(void) {
    CFAbsoluteTime now = CFAbsoluteTimeGetCurrent();
    if (now - g_last_shape_at < 0.25) {
        return;
    }
    g_last_shape_at = now;
    NSCursor *cursor = [NSCursor currentSystemCursor];
    NSImage *image = cursor ? cursor.image : nil;
    if (!image) {
        return;
    }
    CGImageRef cg = [image CGImageForProposedRect:NULL context:nil hints:nil];
    if (!cg) {
        return;
    }
    // 协议里的视频坐标是逻辑点。Retina 上 CGImageForProposedRect 会给出
    // 2 倍 backing 像素，直接发过去光标会大一倍，所以按 image.size 缩回去。
    size_t w = (size_t)lround(image.size.width);
    size_t h = (size_t)lround(image.size.height);
    if (w == 0 || h == 0 || w > 128 || h > 128) {
        return;
    }
    size_t row = w * 4;
    // CGBitmapContext 不会清空调用方给的缓冲区，透明像素必须先归零。
    uint8_t *bgra = calloc(row * h, 1);
    if (!bgra) {
        return;
    }
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef ctx = CGBitmapContextCreate(bgra, w, h, 8, row, cs,
        kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
    CGColorSpaceRelease(cs);
    if (!ctx) {
        free(bgra);
        return;
    }
    CGContextDrawImage(ctx, CGRectMake(0, 0, (CGFloat)w, (CGFloat)h), cg);
    CGContextRelease(ctx);

    NSPoint hot = cursor.hotSpot;
    long hx = lround(hot.x);
    long hy = lround(hot.y);
    if (hx < 0) hx = 0;
    if (hy < 0) hy = 0;
    if (hx > (long)w - 1) hx = (long)w - 1;
    if (hy > (long)h - 1) hy = (long)h - 1;

    int len = 16 + (int)(row * h);
    uint8_t *body = malloc((size_t)len);
    if (!body) {
        free(bgra);
        return;
    }
    put_u16(body, (uint16_t)hx);
    put_u16(body + 2, (uint16_t)hy);
    put_u16(body + 4, (uint16_t)w);
    put_u16(body + 6, (uint16_t)h);
    put_u64(body + 8, shape_hash((uint16_t)hx, (uint16_t)hy, (uint16_t)w, (uint16_t)h, bgra, row * h));
    memcpy(body + 16, bgra, row * h);
    free(bgra);
    pthread_mutex_lock(&g_shape_mu);
    free(g_local_shape);
    g_local_shape = body;
    g_local_shape_len = len;
    pthread_mutex_unlock(&g_shape_mu);
}

void jrp_cursor_shape_request(void) {
    atomic_store(&g_shape_req, 1);
}

int jrp_cursor_shape(uint8_t *out, int cap) {
    if (!out || cap <= 0) {
        return 0;
    }
    pthread_mutex_lock(&g_shape_mu);
    int len = g_local_shape_len;
    if (len > 0 && len <= cap) {
        memcpy(out, g_local_shape, (size_t)len);
    } else {
        len = 0;
    }
    pthread_mutex_unlock(&g_shape_mu);
    return len;
}

void jrp_view_set_cursor(int32_t x, int32_t y, int32_t visible) {
    atomic_store(&g_cursor_x, x);
    atomic_store(&g_cursor_y, y);
    atomic_store(&g_cursor_visible, visible ? 1 : 0);
    atomic_store(&g_cursor_dirty, 1);
}

void jrp_view_set_cursor_shape(const uint8_t *bgra, int w, int h, int hx, int hy) {
    if (!bgra || w <= 0 || h <= 0 || w > 128 || h > 128) {
        return;
    }
    if (hx < 0) hx = 0;
    if (hy < 0) hy = 0;
    if (hx > w - 1) hx = w - 1;
    if (hy > h - 1) hy = h - 1;
    int len = 16 + w * h * 4;
    uint8_t *body = malloc((size_t)len);
    if (!body) {
        return;
    }
    put_u16(body, (uint16_t)hx);
    put_u16(body + 2, (uint16_t)hy);
    put_u16(body + 4, (uint16_t)w);
    put_u16(body + 6, (uint16_t)h);
    put_u64(body + 8, shape_hash((uint16_t)hx, (uint16_t)hy, (uint16_t)w, (uint16_t)h, bgra, (size_t)(w * h * 4)));
    memcpy(body + 16, bgra, (size_t)(w * h * 4));
    pthread_mutex_lock(&g_shape_mu);
    free(g_remote_shape);
    g_remote_shape = body;
    g_remote_shape_len = len;
    pthread_mutex_unlock(&g_shape_mu);
    atomic_store(&g_remote_shape_dirty, 1);
}

/// 主线程把远端 BGRA 转成 RGBA 再做 NSImage。解码和 AppKit 都留在主线程。
static void rebuild_remote_cursor_on_main(void) {
    if (!atomic_exchange(&g_remote_shape_dirty, 0)) {
        return;
    }
    pthread_mutex_lock(&g_shape_mu);
    uint8_t *body = NULL;
    int len = g_remote_shape_len;
    if (g_remote_shape && len >= 16) {
        body = malloc((size_t)len);
        if (body) {
            memcpy(body, g_remote_shape, (size_t)len);
        }
    }
    pthread_mutex_unlock(&g_shape_mu);
    if (!body) {
        return;
    }
    int hx = body[0] | (body[1] << 8);
    int hy = body[2] | (body[3] << 8);
    int w = body[4] | (body[5] << 8);
    int h = body[6] | (body[7] << 8);
    if (w <= 0 || h <= 0 || len != 16 + w * h * 4) {
        free(body);
        return;
    }
    uint8_t *rgba = malloc((size_t)(w * h * 4));
    if (!rgba) {
        free(body);
        return;
    }
    for (int i = 0; i < w * h; i++) {
        rgba[i * 4] = body[16 + i * 4 + 2];
        rgba[i * 4 + 1] = body[16 + i * 4 + 1];
        rgba[i * 4 + 2] = body[16 + i * 4];
        rgba[i * 4 + 3] = body[16 + i * 4 + 3];
    }
    NSBitmapImageRep *rep = [[NSBitmapImageRep alloc]
        initWithBitmapDataPlanes:NULL
                      pixelsWide:w
                      pixelsHigh:h
                   bitsPerSample:8
                 samplesPerPixel:4
                        hasAlpha:YES
                        isPlanar:NO
                  colorSpaceName:NSDeviceRGBColorSpace
                    bitmapFormat:0
                     bytesPerRow:w * 4
                    bitsPerPixel:32];
    if (rep) {
        memcpy(rep.bitmapData, rgba, (size_t)(w * h * 4));
        NSImage *img = [[NSImage alloc] initWithSize:NSMakeSize(w, h)];
        [img addRepresentation:rep];
        g_cursor_image = img;
        atomic_store(&g_cursor_hot_x, hx);
        atomic_store(&g_cursor_hot_y, hy);
    }
    free(rgba);
    free(body);
    if (g_video) {
        [g_video setNeedsDisplay:YES];
    }
}

void jrp_view_pump(void) {
    if (![NSThread isMainThread]) {
        return;
    }
    if (atomic_exchange(&g_open_req, 0)) {
        open_view_on_main();
    }
    apply_quality_on_main();
    apply_displays_on_main();
    if (atomic_exchange(&g_shape_req, 0)) {
        capture_cursor_shape_on_main();
    }
    rebuild_remote_cursor_on_main();
    if (atomic_exchange(&g_cursor_dirty, 0) && g_video) {
        [g_video setNeedsDisplay:YES];
    }
    int ind = atomic_exchange(&g_indicator_req, 0);
    if (ind == 1) {
        indicator_on_main(1);
    } else if (ind == 2) {
        indicator_on_main(0);
    }
    int cover = atomic_exchange(&g_cover_req, 0);
    if (cover == 1) {
        cover_on_main(1);
    } else if (cover == 2) {
        cover_on_main(0);
    }
    pthread_mutex_lock(&g_mu);
    JrpPendingFrame *pending = g_pending_head;
    g_pending_head = NULL;
    g_pending_tail = NULL;
    g_pending_count = 0;
    pthread_mutex_unlock(&g_mu);
    while (pending) {
        JrpPendingFrame *next = pending->next;
        present_annex(pending->bytes, pending->len, pending->idr);
        free(pending->bytes);
        free(pending);
        pending = next;
    }
    if (NSApp) {
        NSDate *until = [NSDate dateWithTimeIntervalSinceNow:0.005];
        NSEvent *event = nil;
        while ((event = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:until inMode:NSDefaultRunLoopMode dequeue:YES])) {
            [NSApp sendEvent:event];
            until = [NSDate date];
        }
    }
}
