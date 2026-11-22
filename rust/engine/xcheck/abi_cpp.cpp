// Prints sizeof/offsetof of the C API structs, in the format of abi::layout_lines() (Rust).
#include <cstddef>
#include <cstdio>
#include "../../../src/engine/EngineApi.h"

#define S(T) printf("sizeof %s %zu\n", #T, sizeof(T))
#define O(T, f) printf("offsetof %s.%s %zu\n", #T, #f, offsetof(T, f))

int main()
{
    S(CsFormat); O(CsFormat, width); O(CsFormat, height); O(CsFormat, fpsNum); O(CsFormat, fpsDen); O(CsFormat, fourcc);
    O(CsFormat, isNative); O(CsFormat, lens); O(CsFormat, lensLabel);
    S(CsImage); O(CsImage, rotation); O(CsImage, mirror); O(CsImage, flip); O(CsImage, aspect); O(CsImage, zoom);
    O(CsImage, centerX); O(CsImage, centerY); O(CsImage, brightness); O(CsImage, contrast); O(CsImage, saturation);
    O(CsImage, temperature);
    S(CsStatus); O(CsStatus, state); O(CsStatus, width); O(CsStatus, height); O(CsStatus, cameraWidth);
    O(CsStatus, cameraHeight); O(CsStatus, cameraFourcc); O(CsStatus, fps); O(CsStatus, processMs);
    O(CsStatus, framesCaptured); O(CsStatus, framesDropped); O(CsStatus, sharedMemory); O(CsStatus, consumerActive);
    O(CsStatus, sourceRotation); O(CsStatus, standby); O(CsStatus, pictureWidth); O(CsStatus, pictureHeight);
    O(CsStatus, sourceId); O(CsStatus, noSource); O(CsStatus, lagMs); O(CsStatus, framesSkipped); O(CsStatus, error);
    S(CsBackground); O(CsBackground, available); O(CsBackground, active); O(CsBackground, inferMs); O(CsBackground, faces);
    S(CsFraming); O(CsFraming, zoom); O(CsFraming, centerX); O(CsFraming, centerY); O(CsFraming, maxZoom);
    O(CsFraming, active); O(CsFraming, faces); O(CsFraming, available); O(CsFraming, cameraSource);
    return 0;
}
