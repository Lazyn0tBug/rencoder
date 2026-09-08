// Rencoder FFI bridge to libavformat for fast video metadata probing.
//
// This is the C side of the optional `c-probe` feature. Each call to
// `rencoder_probe` allocates a fresh `AVFormatContext` and tears it down
// before returning, so the bridge is stateless and safe to call from
// multiple threads (rayon, signal handlers, etc.) without locking.
//
// Returns a heap-allocated struct with malloc'd strings. The Rust side
// reads the struct, parses the strings, and calls `rencoder_free` to
// release the memory.

#include <libavformat/avformat.h>
#include <libavutil/avutil.h>
#include <libavutil/rational.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    char *codec_name;     // malloc'd, NULL on failure
    char *avg_frame_rate; // "num/den" or NULL
    char *r_frame_rate;   // "num/den" or NULL
    double duration;      // seconds, NaN if unavailable
    char *container;      // format.format_name, e.g. "mov,mp4,m4a"
    int width;            // stream width, 0 if unavailable
    int height;           // stream height, 0 if unavailable
    long long bit_rate;   // bits per second, 0 if unavailable
} rencoder_probe_result_t;

static char *dup_or_null(const char *s) {
    if (!s) {
        return NULL;
    }
    char *r = strdup(s);
    return r;
}

static char *rational_to_str(AVRational r) {
    char buf[64];
    if (r.den == 0) {
        // Matches the Rust parser's behavior: "0/0" -> Err via parse_fraction.
        snprintf(buf, sizeof(buf), "0/0");
    } else {
        snprintf(buf, sizeof(buf), "%d/%d", r.num, r.den);
    }
    return strdup(buf);
}

rencoder_probe_result_t *rencoder_probe(const char *path) {
    rencoder_probe_result_t *result = calloc(1, sizeof(*result));
    if (!result) {
        return NULL;
    }
    result->duration = NAN;

    AVFormatContext *fmt_ctx = NULL;
    if (avformat_open_input(&fmt_ctx, path, NULL, NULL) != 0) {
        // Open failed; return a zeroed result so the Rust side sees None
        // fields (treated as a failed probe, surfaced via failed_files).
        return result;
    }

    if (avformat_find_stream_info(fmt_ctx, NULL) < 0) {
        avformat_close_input(&fmt_ctx);
        return result;
    }

    // Find the first video stream.
    int video_idx = -1;
    for (unsigned i = 0; i < fmt_ctx->nb_streams; i++) {
        if (fmt_ctx->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_VIDEO) {
            video_idx = (int)i;
            break;
        }
    }

    if (video_idx >= 0) {
        AVStream *stream = fmt_ctx->streams[video_idx];
        result->codec_name =
            dup_or_null(avcodec_get_name(stream->codecpar->codec_id));
        result->avg_frame_rate = rational_to_str(stream->avg_frame_rate);
        result->r_frame_rate = rational_to_str(stream->r_frame_rate);
        result->width = stream->codecpar->width;
        result->height = stream->codecpar->height;
        if (stream->codecpar->bit_rate > 0) {
            result->bit_rate = stream->codecpar->bit_rate;
        }
        if (fmt_ctx->duration != AV_NOPTS_VALUE) {
            result->duration = (double)fmt_ctx->duration / (double)AV_TIME_BASE;
        }
    }

    // Container (format-level) fields. Some fixtures put these in
    // format.format_name / format.bit_rate.
    if (fmt_ctx->iformat) {
        result->container = dup_or_null(fmt_ctx->iformat->name);
    }
    if (fmt_ctx->bit_rate > 0 && result->bit_rate == 0) {
        result->bit_rate = fmt_ctx->bit_rate;
    }

    avformat_close_input(&fmt_ctx);
    return result;
}

void rencoder_free(rencoder_probe_result_t *result) {
    if (!result) {
        return;
    }
    free(result->codec_name);
    free(result->avg_frame_rate);
    free(result->r_frame_rate);
    free(result->container);
    free(result);
}
