awww-about(1)

# NAME
awww-about

# SYNOPSIS
*awww about* [_OPTIONS_]

# OPTIONS

*-j*,*--json*
	Print the information in *json* format.

*-h*, *--help*
	Print help (see a summary with '-h')

# DESCRIPTION

Prints information regarding version and supported formats.

You may use this to find out version information, what static formats are supported, and what animated formats are supported.

# EXAMPLES

By default prints information as lines with `:` separating keys from values and values being comma-delimited.

	*awww about*++
	Version: 0.13.0++
	Supported static formats: avif, bmp, dds, exr, ff, gif, hdr, ico, jpeg,
	jxl, png, pnm, qoi, tga, tiff++
	Supported animated formats: gif, png, webp

Specifying *-j* will print the json response with `version` as a string and
supported formats as an array of strings.
