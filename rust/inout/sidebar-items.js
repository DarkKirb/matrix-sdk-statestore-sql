window.SIDEBAR_ITEMS = {"struct":[["InOut","Custom pointer type which contains one immutable (input) and one mutable (output) pointer, which are either equal or non-overlapping."],["InOutBuf","Custom slice type which references one immutable (input) slice and one mutable (output) slice of equal length. Input and output slices are either the same or do not overlap."],["InOutBufIter","Iterator over [`InOutBuf`]."],["InOutBufReserved","Custom slice type which references one immutable (input) slice and one mutable (output) slice. Input and output slices are either the same or do not overlap. Length of the output slice is always equal or bigger than length of the input slice."],["IntoArrayError","The error returned when slice can not be converted into array."],["NotEqualError","The error returned when input and output slices have different length and thus can not be converted to `InOutBuf`."],["OutIsTooSmallError","Output buffer is smaller than input buffer."],["PadError","Padding error. Usually emitted when size of output buffer is insufficient."],["PaddedInOutBuf","Variant of [`InOutBuf`] with optional padded tail block."]]};