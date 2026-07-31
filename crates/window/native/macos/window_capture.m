// Captures a macOS window or the whole screen with ScreenCaptureKit and writes
// the pixels to a file descriptor.
//
// ## Why this is linked in rather than spawned
//
// It began as a helper process, which does not work: macOS attributes Screen
// Recording permission to the *responsible* application, and an unsigned binary
// unpacked into a temporary directory is nobody's responsibility. The user
// could approve their terminal as many times as they liked and the helper would
// still be told the user declined, because the approval was never about it.
// Compiled into the Rust binary, the request belongs to whatever launched
// Cursio — the terminal the user already approved.
//
// No `NSApplication` either, for the same class of reason: this is a library
// inside somebody else's process, and it must not take over the main thread.
// `SCStream` delivers on a dispatch queue, which is all it needs.
//
// Wire format written to `out_fd`, repeated per frame:
//
//   width:  u32 big-endian
//   height: u32 big-endian
//   length: u32 big-endian   (always width * height * 4)
//   pixels: BGRA, tightly packed, no row padding
//
// Rows are repacked here because CVPixelBuffer rows are padded to a stride the
// GPU likes, while Cursio's wire format has none. Doing it here keeps one copy
// out of the Rust side.

#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <stdint.h>
#include <string.h>
#include <unistd.h>

/// Matches `CaptureError` on the Rust side.
enum {
    CURSIO_CAPTURE_OK = 0,
    CURSIO_CAPTURE_DENIED = -1,
    CURSIO_CAPTURE_NO_MATCH = -2,
    CURSIO_CAPTURE_FAILED = -3,
};

static BOOL write_exact(int fd, const void *buffer, size_t length) {
    const uint8_t *cursor = buffer;
    while (length != 0) {
        ssize_t count = write(fd, cursor, length);
        if (count <= 0) {
            if (count < 0 && errno == EINTR) {
                continue;
            }
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

@interface CursioCapture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, strong) SCStream *stream;
@property(nonatomic, assign) int fd;
@end

@implementation CursioCapture

- (void)stream:(SCStream *)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen || self.fd < 0) {
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
        BOOL ok = write_exact(self.fd, header, sizeof(header));
        for (size_t row = 0; ok && row < height; row++) {
            ok = write_exact(self.fd, base + row * stride, row_bytes);
        }
        if (!ok) {
            // The reader closed the pipe; there is nothing to capture for.
            close(self.fd);
            self.fd = -1;
            [self.stream stopCaptureWithCompletionHandler:^(NSError *error) {
              (void)error;
            }];
        }
    }
    CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    NSLog(@"cursio: screen capture stopped: %@", error.localizedDescription);
    if (self.fd >= 0) {
        close(self.fd);
        self.fd = -1;
    }
}

@end

/// Pick the window whose title contains `needle`. Never called with an empty
/// needle: sharing without naming a window means the whole screen, because
/// "whichever window the system lists first" is not something a user can want.
static SCWindow *choose_window(SCShareableContent *content, NSString *needle) {
    for (SCWindow *window in content.windows) {
        if (window.title.length == 0 || window.frame.size.width < 2) {
            continue;
        }
        if ([window.title rangeOfString:needle options:NSCaseInsensitiveSearch]
                .location != NSNotFound) {
            return window;
        }
    }
    return nil;
}

/// Keeps the capture alive for the life of the process. One window is shared at
/// a time, so one slot is enough; a second call replaces the first.
static CursioCapture *g_capture = nil;

/// Start capturing, writing frames to `out_fd` until the reader closes it.
///
/// Blocks until the stream is running or has failed, so the caller learns about
/// a refusal here rather than by waiting forever for a frame that never comes.
/// `out_fd` is owned by this function once it succeeds.
int cursio_window_capture_start(const char *needle_utf8, int fps, int out_fd) {
    @autoreleasepool {
        NSString *needle = needle_utf8 != NULL ? @(needle_utf8) : @"";
        __block int result = CURSIO_CAPTURE_FAILED;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);

        [SCShareableContent
            getShareableContentWithCompletionHandler:^(SCShareableContent *content,
                                                       NSError *error) {
              if (error != nil) {
                  NSLog(@"cursio: cannot list windows: %@", error.localizedDescription);
                  result = CURSIO_CAPTURE_DENIED;
                  dispatch_semaphore_signal(done);
                  return;
              }

              SCContentFilter *filter = nil;
              size_t width = 0;
              size_t height = 0;
              if (needle.length == 0) {
                  SCDisplay *display = content.displays.firstObject;
                  if (display == nil) {
                      result = CURSIO_CAPTURE_NO_MATCH;
                      dispatch_semaphore_signal(done);
                      return;
                  }
                  NSLog(@"cursio: capturing the whole screen %zux%zu",
                        (size_t)display.width, (size_t)display.height);
                  filter = [[SCContentFilter alloc] initWithDisplay:display
                                                   excludingWindows:@[]];
                  width = (size_t)display.width;
                  height = (size_t)display.height;
              } else {
                  SCWindow *window = choose_window(content, needle);
                  if (window == nil) {
                      NSLog(@"cursio: no window matched \"%@\"", needle);
                      result = CURSIO_CAPTURE_NO_MATCH;
                      dispatch_semaphore_signal(done);
                      return;
                  }
                  NSLog(@"cursio: capturing \"%@\" %.0fx%.0f", window.title,
                        window.frame.size.width, window.frame.size.height);
                  filter = [[SCContentFilter alloc]
                      initWithDesktopIndependentWindow:window];
                  width = (size_t)window.frame.size.width;
                  height = (size_t)window.frame.size.height;
              }

              SCStreamConfiguration *configuration = [SCStreamConfiguration new];
              configuration.width = width;
              configuration.height = height;
              configuration.pixelFormat = kCVPixelFormatType_32BGRA;
              configuration.showsCursor = NO;
              // A ceiling, not a rate: frames arrive only when something
              // changed, so a still window costs nothing.
              configuration.minimumFrameInterval = CMTimeMake(1, fps);
              configuration.queueDepth = 3;

              CursioCapture *capture = [CursioCapture new];
              capture.fd = out_fd;
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
                  NSLog(@"cursio: cannot attach capture output: %@",
                        addError.localizedDescription);
                  result = CURSIO_CAPTURE_FAILED;
                  dispatch_semaphore_signal(done);
                  return;
              }
              [capture.stream startCaptureWithCompletionHandler:^(NSError *startError) {
                if (startError != nil) {
                    NSLog(@"cursio: cannot start capture: %@",
                          startError.localizedDescription);
                    result = CURSIO_CAPTURE_DENIED;
                } else {
                    g_capture = capture;
                    result = CURSIO_CAPTURE_OK;
                }
                dispatch_semaphore_signal(done);
              }];
            }];

        // Ten seconds covers the permission dialog appearing and being answered.
        if (dispatch_semaphore_wait(
                done, dispatch_time(DISPATCH_TIME_NOW, 10ll * NSEC_PER_SEC)) != 0) {
            NSLog(@"cursio: screen capture did not start in time");
            return CURSIO_CAPTURE_FAILED;
        }
        return result;
    }
}

/// Stop the running capture, if any, and close its pipe.
void cursio_window_capture_stop(void) {
    CursioCapture *capture = g_capture;
    g_capture = nil;
    if (capture == nil) {
        return;
    }
    [capture.stream stopCaptureWithCompletionHandler:^(NSError *error) {
      (void)error;
    }];
    if (capture.fd >= 0) {
        close(capture.fd);
        capture.fd = -1;
    }
}
