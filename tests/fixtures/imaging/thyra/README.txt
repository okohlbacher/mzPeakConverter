Pixel size test files for imaging converters
============================================

Five small synthetic imzML files (each an .imzML and .ibd pair). Every file
holds the same acquisition: 3 by 2 pixels, each 50 um wide, six profile
spectra on one shared m/z axis, declared as a meandering scan. Only the way
the header states the pixel size differs. The five ways are the ones found
in the headers of public imzML files.

File                                  What the header gives
------------------------------------  -----------------------------------------
pixel_size_unit_declared              IMS:1000046 and IMS:1000047, value 50,
                                      unit micrometre, and the extent
pixel_size_unit_absent                both terms, value 50, no unit, and the
                                      extent
pixel_size_area_old_name              IMS:1000046 alone, named "pixel size",
                                      value 2500, no unit, and the extent
pixel_size_area_old_name_no_extent    the same, without the extent
pixel_size_unit_contradiction         IMS:1000046 alone, named "pixel size",
                                      value 50, unit accession UO:0000015
                                      (centimetre), unit name "micrometer",
                                      no extent

The extent is IMS:1000044 and IMS:1000045: 150 um by 100 um.

Until 2017, IMS:1000046 was named "pixel size" and gave the AREA of a pixel
(imzML vocabulary commit 421481e, 2017-09-07). So 2500 in the third and
fourth file is 50 um times 50 um.

pixel_size_expected.json gives, for each file, what a reader can conclude
from the header alone:

  pixel_size_um       50 on both axes, or null
  pixel_size_source   declared           both terms with a unit
                      unit_assumed       the value is there, micrometre assumed
                      derived_from_area  square root of the value, because the
                                         square root times the pixel count is
                                         the extent
                      unknown            the header does not settle it

Where they come from
--------------------
The same files are in the Thyra repository, with the script that writes them:

  https://github.com/M4i-Imaging-Mass-Spectrometry/thyra/tree/main/tests/data/fixtures

They are synthetic, hold no real measurement, and are under Thyra's MIT
licence. Use them freely.

Theodoros Visvikis, Maastricht University (M4i), 2026-09-30
