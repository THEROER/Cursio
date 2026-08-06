#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <QuartzCore/QuartzCore.h>

#include <errno.h>
#include <stdatomic.h>
#include <stdint.h>
#include <unistd.h>

// The reader thread outruns AppKit whenever presentation stalls. Without a cap
// the main queue accumulates access units nobody will see, trading latency and
// memory for frames that are already stale by the time they are decoded.
#define CURSIO_MAX_PENDING_UNITS 8

static _Atomic int pending_units = 0;

static BOOL read_exact(int fd, void *buffer, size_t length) {
    uint8_t *cursor = buffer;
    while (length != 0) {
        ssize_t count = read(fd, cursor, length);
        if (count == 0) {
            return NO;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            return NO;
        }
        cursor += count;
        length -= (size_t)count;
    }
    return YES;
}

static NSArray<NSData *> *annex_b_units(NSData *access_unit) {
    const uint8_t *bytes = access_unit.bytes;
    const NSUInteger length = access_unit.length;
    NSMutableArray<NSData *> *units = [NSMutableArray array];
    NSUInteger cursor = 0;

    while (cursor + 3 < length) {
        NSUInteger marker = NSNotFound;
        NSUInteger marker_length = 0;
        for (NSUInteger index = cursor; index + 3 < length; index++) {
            if (bytes[index] == 0 && bytes[index + 1] == 0 &&
                bytes[index + 2] == 1) {
                marker = index;
                marker_length = 3;
                break;
            }
            if (index + 4 <= length && bytes[index] == 0 &&
                bytes[index + 1] == 0 && bytes[index + 2] == 0 &&
                bytes[index + 3] == 1) {
                marker = index;
                marker_length = 4;
                break;
            }
        }
        if (marker == NSNotFound) {
            break;
        }

        NSUInteger start = marker + marker_length;
        NSUInteger end = length;
        for (NSUInteger index = start; index + 3 < length; index++) {
            if (bytes[index] == 0 && bytes[index + 1] == 0 &&
                (bytes[index + 2] == 1 ||
                 (index + 4 <= length && bytes[index + 2] == 0 &&
                  bytes[index + 3] == 1))) {
                end = index;
                break;
            }
        }
        if (end > start) {
            [units addObject:[access_unit subdataWithRange:NSMakeRange(start, end - start)]];
        }
        cursor = end;
    }
    return units;
}

@interface CursioVideoDelegate : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) AVSampleBufferDisplayLayer *displayLayer;
@property(nonatomic, strong) NSData *sps;
@property(nonatomic, strong) NSData *pps;
@end

@implementation CursioVideoDelegate {
    CMVideoFormatDescriptionRef _format;
}

- (void)dealloc {
    if (_format != NULL) {
        CFRelease(_format);
    }
}

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    (void)notification;
    NSRect frame = NSMakeRect(0, 0, 960, 540);
    NSWindow *window = [[NSWindow alloc]
        initWithContentRect:frame
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                            NSWindowStyleMaskResizable | NSWindowStyleMaskMiniaturizable
                    backing:NSBackingStoreBuffered
                      defer:NO];
    window.title = @"Cursio - AirPlay Screen";
    window.releasedWhenClosed = NO;

    NSView *content = window.contentView;
    content.wantsLayer = YES;
    content.layer = [CALayer layer];
    content.layer.backgroundColor = NSColor.blackColor.CGColor;

    self.displayLayer = [AVSampleBufferDisplayLayer layer];
    self.displayLayer.frame = content.layer.bounds;
    self.displayLayer.autoresizingMask = kCALayerWidthSizable | kCALayerHeightSizable;
    self.displayLayer.videoGravity = AVLayerVideoGravityResizeAspect;
    [content.layer addSublayer:self.displayLayer];

    [window center];
    [window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];

    __weak CursioVideoDelegate *weakSelf = self;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0), ^{
        for (;;) {
            uint8_t size_bytes[4];
            if (!read_exact(STDIN_FILENO, size_bytes, sizeof(size_bytes))) {
                break;
            }
            uint32_t size = ((uint32_t)size_bytes[0] << 24) |
                            ((uint32_t)size_bytes[1] << 16) |
                            ((uint32_t)size_bytes[2] << 8) |
                            (uint32_t)size_bytes[3];
            if (size == 0 || size > 32 * 1024 * 1024) {
                break;
            }
            NSMutableData *data = [NSMutableData dataWithLength:size];
            if (!read_exact(STDIN_FILENO, data.mutableBytes, size)) {
                break;
            }
            if (atomic_load(&pending_units) >= CURSIO_MAX_PENDING_UNITS) {
                continue;
            }
            atomic_fetch_add(&pending_units, 1);
            dispatch_async(dispatch_get_main_queue(), ^{
                [weakSelf consumeAccessUnit:data];
                atomic_fetch_sub(&pending_units, 1);
            });
        }
        dispatch_async(dispatch_get_main_queue(), ^{
            [NSApp terminate:nil];
        });
    });
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
    (void)sender;
    return YES;
}

- (void)refreshFormat {
    if (self.sps == nil || self.pps == nil) {
        return;
    }
    if (_format != NULL) {
        CFRelease(_format);
        _format = NULL;
    }
    const uint8_t *sets[] = {self.sps.bytes, self.pps.bytes};
    const size_t sizes[] = {self.sps.length, self.pps.length};
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
        kCFAllocatorDefault, 2, sets, sizes, 4, &_format);
    if (status != noErr) {
        _format = NULL;
    }
}

- (void)consumeAccessUnit:(NSData *)accessUnit {
    NSArray<NSData *> *units = annex_b_units(accessUnit);
    if (units.count == 0) {
        return;
    }

    BOOL formatChanged = NO;
    NSMutableData *sampleData = [NSMutableData data];
    for (NSData *unit in units) {
        const uint8_t *bytes = unit.bytes;
        if (unit.length == 0) {
            continue;
        }
        uint8_t type = bytes[0] & 0x1f;
        if (type == 7) {
            self.sps = unit;
            formatChanged = YES;
            continue;
        }
        if (type == 8) {
            self.pps = unit;
            formatChanged = YES;
            continue;
        }
        uint32_t length = CFSwapInt32HostToBig((uint32_t)unit.length);
        [sampleData appendBytes:&length length:sizeof(length)];
        [sampleData appendData:unit];
    }
    if (formatChanged) {
        [self refreshFormat];
    }
    if (_format == NULL || sampleData.length == 0) {
        return;
    }

    CMBlockBufferRef block = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(
        kCFAllocatorDefault, NULL, sampleData.length, kCFAllocatorDefault, NULL,
        0, sampleData.length, 0, &block);
    if (status != noErr) {
        return;
    }
    status = CMBlockBufferReplaceDataBytes(
        sampleData.bytes, block, 0, sampleData.length);
    if (status != noErr) {
        CFRelease(block);
        return;
    }

    CMSampleBufferRef sample = NULL;
    size_t sampleSize = sampleData.length;
    status = CMSampleBufferCreateReady(
        kCFAllocatorDefault, block, _format, 1, 0, NULL, 1, &sampleSize, &sample);
    CFRelease(block);
    if (status != noErr) {
        return;
    }
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, true);
    if (attachments != NULL && CFArrayGetCount(attachments) != 0) {
        CFMutableDictionaryRef attachment =
            (CFMutableDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
        CFDictionarySetValue(
            attachment, kCMSampleAttachmentKey_DisplayImmediately, kCFBooleanTrue);
    }
    if (self.displayLayer.status == AVQueuedSampleBufferRenderingStatusFailed) {
        [self.displayLayer flush];
    }
    [self.displayLayer enqueueSampleBuffer:sample];
    CFRelease(sample);
}

@end

int main(void) {
    @autoreleasepool {
        NSApplication *application = [NSApplication sharedApplication];
        application.activationPolicy = NSApplicationActivationPolicyRegular;
        CursioVideoDelegate *delegate = [CursioVideoDelegate new];
        application.delegate = delegate;
        [application run];
    }
    return 0;
}
