/*++

Copyright (c) Microsoft Corporation.  All rights reserved.

    THIS CODE AND INFORMATION IS PROVIDED "AS IS" WITHOUT WARRANTY OF ANY
    KIND, EITHER EXPRESSED OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
    IMPLIED WARRANTIES OF MERCHANTABILITY AND/OR FITNESS FOR A PARTICULAR
    PURPOSE.

Module Name:

    DriverSettings.h

Abstract:

    Contains guid definitions and other definitions used by the render and capture circuits
    for this specific driver. Driver developers should replace these definitions with their
    own. 

Environment:

    Kernel mode

--*/

// Cursio: unique component GUIDs (replacing the sample's, per MS-PL guidance to
// use your own). Capture circuit component id:
DEFINE_GUID(CODEC_CAPTURE_COMPONENT_GUID, 0x6f552e0c, 0x2b73, 0x4903, 0x9b, 0x93, 0x20, 0xf5, 0x4f, 0x55, 0x55, 0x5e);

// Cursio: custom name for the capture circuit bridge pin:
DEFINE_GUID(MIC_CUSTOM_NAME, 0x8ff89a78, 0xe8f8, 0x4622, 0xb0, 0xb1, 0x69, 0x5f, 0x23, 0x6c, 0xad, 0xc6);

// Cursio: render circuit component id:
DEFINE_GUID(CODEC_RENDER_COMPONENT_GUID, 0xce0a9f26, 0x4cbb, 0x4359, 0x89, 0x29, 0xb3, 0x3b, 0x45, 0x6a, 0x61, 0x0b);

// This is always the definition for the system container guid:
DEFINE_GUID(SYSTEM_CONTAINER_GUID, 0x00000000, 0x0000, 0x0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF);

// Driver developers should update this guid if the container is a device rather than a
// system. Otherwise, this GUID should stay the same:
DEFINE_GUID(DEVICE_CONTAINER_GUID, 0x00000000, 0x0000, 0x0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF);

// Cursio driver pool tag ('oisC' = "Cursio" reversed, shown in pool dumps):
#define DRIVER_TAG (ULONG) 'oisC'

// The idle timeout in msec for power policy structure:
#define IDLE_TIMEOUT_MSEC (ULONG) 10000

// The WPP control GUID defined in Trace.h should also be updated to be unique.

// This string must match the string defined in AudioCodec.inf for the capture name:
DECLARE_CONST_UNICODE_STRING(captureCircuitName, L"Cursio Audio Capture");

// This string must match the string defined in AudioCodec.inf for the render name:
DECLARE_CONST_UNICODE_STRING(renderCircuitName, L"Cursio Audio Render");
