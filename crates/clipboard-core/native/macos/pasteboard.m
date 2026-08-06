// NSPasteboard, reduced to a C surface the Rust backend can call.
//
// AppKit is Objective-C and the clipboard is one of the parts with no C API at
// all, so this file exists for the same reason `cursio-window`'s capture
// helper does: the messaging has to happen somewhere, and doing it here is far
// less error-prone than hand-rolling `objc_msgSend` casts on the Rust side.
//
// Everything is two-phase — ask for a length, then copy into a buffer the
// caller owns. That keeps ownership of every allocation on the Rust side and
// means nothing here has to be freed across the boundary.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>

// The pasteboard's revision. macOS offers no change notification of any kind
// for the pasteboard — polling this counter is the documented way, and what
// every clipboard manager on the platform does.
long long cursio_clipboard_change_count(void) {
  @autoreleasepool {
    return (long long)[[NSPasteboard generalPasteboard] changeCount];
  }
}

// The available types, NUL-separated, into `out`. Returns the bytes needed,
// which may exceed `cap` — the caller then retries with a bigger buffer, so a
// pasteboard with unusually many representations is not silently truncated.
long cursio_clipboard_types(char *out, long cap) {
  @autoreleasepool {
    NSArray<NSPasteboardType> *types = [[NSPasteboard generalPasteboard] types];
    long needed = 0;
    for (NSPasteboardType type in types) {
      const char *utf8 = [type UTF8String];
      if (utf8 == NULL) {
        continue;
      }
      long len = (long)strlen(utf8) + 1;
      if (out != NULL && needed + len <= cap) {
        memcpy(out + needed, utf8, (size_t)len);
      }
      needed += len;
    }
    return needed;
  }
}

// Byte length of one representation, or -1 when the pasteboard does not have
// it. macOS has no way to ask for a size without materialising the data, so
// this does load it; the copy is simply never handed out.
long cursio_clipboard_size(const char *uti) {
  @autoreleasepool {
    NSString *type = [NSString stringWithUTF8String:uti];
    if (type == nil) {
      return -1;
    }
    NSData *data = [[NSPasteboard generalPasteboard] dataForType:type];
    return data == nil ? -1 : (long)[data length];
  }
}

// Copy one representation into `out`. Returns the bytes written, or -1 if the
// type is gone or the buffer is too small — which can happen legitimately
// between a size call and this one, because another application may replace
// the pasteboard at any moment.
long cursio_clipboard_read(const char *uti, unsigned char *out, long cap) {
  @autoreleasepool {
    NSString *type = [NSString stringWithUTF8String:uti];
    if (type == nil) {
      return -1;
    }
    NSData *data = [[NSPasteboard generalPasteboard] dataForType:type];
    if (data == nil || (long)[data length] > cap) {
      return -1;
    }
    memcpy(out, [data bytes], [data length]);
    return (long)[data length];
  }
}

// Take ownership of the pasteboard, discarding what was there. Returns the new
// change count, which the writer records so its own write is not mistaken for
// somebody else's copy.
long long cursio_clipboard_clear(void) {
  @autoreleasepool {
    return (long long)[[NSPasteboard generalPasteboard] clearContents];
  }
}

// Add one representation to the pasteboard. Must follow a clear.
int cursio_clipboard_write(const char *uti, const unsigned char *bytes, long len) {
  @autoreleasepool {
    NSString *type = [NSString stringWithUTF8String:uti];
    if (type == nil) {
      return 0;
    }
    NSData *data = [NSData dataWithBytes:bytes length:(NSUInteger)len];
    return [[NSPasteboard generalPasteboard] setData:data forType:type] ? 1 : 0;
  }
}
