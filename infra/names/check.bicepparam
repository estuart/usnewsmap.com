// What scripts/ci/check-names.py checks (ADR-0017, "Enforcement"): names()
// evaluated offline by `bicep build-params`, with the tables it is checked
// against. Nothing here is deployed.
using none

import {
  createOnlyKeys as createOnly
  functionWords as words
  legacyProdSuffix as legacySuffix
  names
  nameTypes as types
  typeAbbreviations as abbreviations
} from '../naming.bicep'

// The environments evaluated in ADR-0017's form, comma separated
// (check-names.py --envs), with a fixed sample suffix, as after a rebuild.
var envs = split(readEnvironmentVariable('USNM_CHECK_ENVS', 'prod,dev,xabcd'), ',')
param sampleSuffix = 'abc123'

// The current prod, pinned: infra/names/prod.json must equal it.
param legacyProdSuffix = legacySuffix
param prodPinned = names('prod', legacySuffix, true)
param conventional = toObject(envs, e => e, e => names(e, sampleSuffix, false))
param functionWords = words
param typeAbbreviations = abbreviations
param nameTypes = types
param createOnlyKeys = createOnly
