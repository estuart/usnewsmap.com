// The BOINC master host's records in the app's public DNS zone (infra/env/<env>.json
// USNM_DNS_ZONE), deployed by the BOINC stack at the zone's group so the records live with
// the BOINC infrastructure: a CNAME to the Container App and the asuid TXT record that
// Container Apps checks before it binds the name.
param zoneName string
@description('The record name in the zone, for example boinc for boinc.usnewsmap.com.')
param recordName string
param appFqdn string
param verificationId string

resource zone 'Microsoft.Network/dnsZones@2018-05-01' existing = {
  name: zoneName
}

resource cname 'Microsoft.Network/dnsZones/CNAME@2018-05-01' = {
  parent: zone
  name: recordName
  properties: { TTL: 3600, CNAMERecord: { cname: appFqdn } }
}

resource asuid 'Microsoft.Network/dnsZones/TXT@2018-05-01' = {
  parent: zone
  name: 'asuid.${recordName}'
  properties: { TTL: 3600, TXTRecords: [ { value: [ verificationId ] } ] }
}
