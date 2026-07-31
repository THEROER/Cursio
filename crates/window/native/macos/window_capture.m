// Captures one macOS window with ScreenCaptureKit and writes its pixels to
// stdout, one frame at a time.
//
// Why a helper process rather than bindings: ScreenCaptureKit is an
// asynchronous Objective-C API built on delegates, blocks and a run loop, and
// the frames arrive as CMSampleBuffers that must be locked and copied on the
// callback queue. Wrapping that in Rust would mean hand-written bindings for a
// framework that already has a perfectly good C entry point — and this crate
// already spawns an Objective-C helper for AirPlay presentation, so the shape
// is the established one here rather than a new idea.
//
// Wire format, repeated per frame:
//
//   width:  u32 big-endian
//   height: u32 big-endian
//   length: u32 big-endian   (always width * height * 4)
//   pixels: BGRA, tightly packed, no row padding
//
// Rows are repacked here because CVPixelBuffer rows are padded to a stride the
// GPU likes, while the wire format Cursio uses has no padding. Doing it in the
// helper keeps one copy out of the Rust side.

#import <AppKit/AppKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static BOOL write_exact(int fd, const void *buffer, size_t length) {
    const uint8_t *cursor = buffer;
    while (length != 0) {
        ssize_t count = write(fd, cursor, length);
        if (count <= 0) {
            return NO;
        }
        cursor += count;
        length -= (size_t)count;
    }
    return YES;
}

static void put_be32(uint8_t *out, uint32_t value) {
    out[0] = (uint8_t)(value >> 24);
    out[1] = (uint8_t)(value >> 16);
    out[2] = (uint8_t)(value >> 8);
    out[3] = (uint8_t)value;
}

@interface CursioWindowCapture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, strong) SCStream *stream;
@end

@implementation CursioWindowCapture

- (void)stream:(SCStream *)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen) {
        return;
    }
    CVImageBufferRef image = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (image == NULL) {
        return;
    }
    if (CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
        return;
    }

    const size_t width = CVPixelBufferGetWidth(image);
    const size_t height = CVPixelBufferGetHeight(image);
    const size_t stride = CVPixelBufferGetBytesPerRow(image);
    const uint8_t *base = CVPixelBufferGetBaseAddress(image);
    const size_t row_bytes = width * 4;

    if (base != NULL && width != 0 && height != 0) {
        uint8_t header[12];
        put_be32(header, (uint32_t)width);
        put_be32(header + 4, (uint32_t)height);
        put_be32(header + 8, (uint32_t)(row_bytes * height));
        if (!write_exact(STDOUT_FILENO, header, sizeof(header))) {
            CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
            [NSApp terminate:nil];
            return;
        }
        for (size_t row = 0; row < height; row++) {
            if (!write_exact(STDOUT_FILENO, base + row * stride, row_bytes)) {
                CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
                // The reader is gone; there is nothing left to capture for.
                [NSApp terminate:nil];
                return;
            }
        }
    }
    CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    fprintf(stderr, "cursio: capture stopped: %s\n",
            error.localizedDescription.UTF8String);
    [NSApp terminate:nil];
}

@end

/// Pick the window to capture: the first whose title contains `needle`, or the
/// frontmost ordinary window when `needle` is empty.
static SCWindow *choose_window(SCShareableContent *content, NSString *needle) {
    for (SCWindow *window in content.windows) {
        if (window.title.length == 0 || window.frame.size.width < 2) {
            continue;
        }
        if (needle.length == 0) {
            return window;
        }
        if ([window.title rangeOfString:needle
                                options:NSCaseInsensitiveSearch]
                .location != NSNotFound) {
            return window;
        }
    }
    return nil;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        NSString *needle = argc > 1 ? @(argv[1]) : @"";
        const int fps = argc > 2 ? atoi(argv[2]) : 30;

        NSApplication *application = [NSApplication sharedApplication];
        application.activationPolicy = NSApplicationActivationPolicyAccessory;
        CursioWindowCapture *capture = [CursioWindowCapture new];

        [SCShareableContent
            getShareableContentWithCompletionHandler:^(SCShareableContent *content,
                                                       NSError *error) {
              if (error != nil) {
                  fprintf(stderr, "cursio: cannot list windows: %s\n",
                          error.localizedDescription.UTF8String);
                  fprintf(stderr,
                          "cursio: grant Screen Recording to the app that "
                          "launched this, in System Settings > Privacy & "
                          "Security\n");
                  exit(2);
              }
              SCWindow *window = choose_window(content, needle);
              if (window == nil) {
                  fprintf(stderr, "cursio: no window matched \"%s\"\n",
                          needle.UTF8String);
                  exit(3);
              }
              fprintf(stderr, "cursio: capturing \"%s\" %.0fx%.0f\n",
                      window.title.UTF8String, window.frame.size.width,
                      window.frame.size.height);

              SCContentFilter *filter =
                  [[SCContentFilter alloc] initWithDesktopIndependentWindow:window];
              SCStreamConfiguration *configuration = [SCStreamConfiguration new];
              configuration.width = (size_t)window.frame.size.width;
              configuration.height = (size_t)window.frame.size.height;
              configuration.pixelFormat = kCVPixelFormatType_32BGRA;
              configuration.showsCursor = NO;
              // Frames arrive only when something changed, so this is a ceiling
              // rather than a rate: a still window costs nothing.
              configuration.minimumFrameInterval = CMTimeMake(1, fps);
              configuration.queueDepth = 3;

              capture.stream = [[SCStream alloc] initWithFilter:filter
                                                  configuration:configuration
                                                       delegate:capture];
              NSError *addError = nil;
              [capture.stream addStreamOutput:capture
                                         type:SCStreamOutputTypeScreen
                           sampleHandlerQueue:dispatch_get_global_queue(
                                                  QOS_CLASS_USER_INTERACTIVE, 0)
                                        error:&addError];
              if (addError != nil) {
                  fprintf(stderr, "cursio: cannot attach output: %s\n",
                          addError.localizedDescription.UTF8String);
                  exit(4);
              }
              [capture.stream startCaptureWithCompletionHandler:^(NSError *startError) {
                if (startError != nil) {
                    fprintf(stderr, "cursio: cannot start capture: %s\n",
                            startError.localizedDescription.UTF8String);
                    exit(5);
                }
              }];
            }];

        [application run];
    }
    return 0;
}
